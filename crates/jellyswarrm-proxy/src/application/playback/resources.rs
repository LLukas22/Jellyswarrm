//! Delivery/resource selection before target-bound request construction.

use super::services::PlaybackService;
#[cfg(test)]
use super::target::matching_session as matching_play_session;
use super::target::{MediaTargetResolver, PlaybackIntent, ResolutionError, SelectionPolicy};
use crate::application::error::ApplicationError;
#[cfg(test)]
use hyper::StatusCode;
use tracing::info;

#[cfg(test)]
use crate::AppState;
use crate::{
    application::playback::sessions::PlaybackSession,
    application::{
        context::{AuthorizedTarget, RequestContext},
        input::OperationRequest,
    },
    auth::identity::JellyfinAuthorization,
    config::MediaStreamingMode,
    servers::storage::Server,
    upstream::jellyfin::{
        delivery::{self, DeliveryResponse},
        request::UpstreamRequestBuilder,
    },
};

fn extract_stream_item_id(path: &str) -> Option<&str> {
    let mut segments = path.trim_matches('/').split('/');

    while let Some(segment) = segments.next() {
        if segment.eq_ignore_ascii_case("Videos") || segment.eq_ignore_ascii_case("Audio") {
            return segments.next().filter(|segment| !segment.is_empty());
        }
    }

    None
}

fn extract_play_session_id(url: &url::Url) -> Result<Option<String>, ApplicationError> {
    crate::processors::url_processor::play_session_id(url).map_err(ApplicationError::InvalidRouting)
}

async fn selected_play_session(
    state: &PlaybackService<'_>,
    operation: &OperationRequest,
    item_id: &str,
    session_id: Option<&str>,
) -> Result<Option<PlaybackSession>, ApplicationError> {
    let Some(user_id) = request_user_id(&operation.context) else {
        return Ok(None);
    };
    let explicit_source =
        crate::processors::url_processor::selected_media_source(operation.original_request.url())
            .map_err(ApplicationError::InvalidRouting)?
            .is_some();
    (MediaTargetResolver {
        media: state.media_storage,
    })
    .session(
        state.play_sessions,
        item_id,
        session_id,
        user_id,
        explicit_source,
    )
    .await
    .map_err(resource_resolution_status)
}

fn resource_resolution_status(error: ResolutionError) -> ApplicationError {
    ApplicationError::PlaybackResolution(error)
        .at(crate::application::error::FailureStage::DeliverySelection)
}

async fn delivery_target(
    state: &PlaybackService<'_>,
    operation: &mut OperationRequest,
    initial: AuthorizedTarget,
    item: &str,
    binding: Option<&PlaybackSession>,
    policy: SelectionPolicy,
) -> Result<AuthorizedTarget, ApplicationError> {
    let source =
        crate::processors::url_processor::selected_media_source(operation.original_request.url())
            .map_err(ApplicationError::InvalidRouting)?;
    let plan = (MediaTargetResolver {
        media: state.media_storage,
    })
    .plan(
        &operation.context,
        initial,
        PlaybackIntent {
            item,
            source: source.as_deref(),
            binding,
        },
        state.server_storage,
        policy,
    )
    .await;
    if matches!(plan, Err(ResolutionError::MissingOwner)) {
        if let Some(binding) = binding {
            super::lifecycle::PlaybackLifecycle::new(state.play_sessions)
                .remove_server(binding.server_id)
                .await;
        }
    }
    let plan = plan.map_err(resource_resolution_status)?;
    tracing::debug!(reason = ?plan.reason, "Playback delivery planned");
    if let Some(member) = plan.target.member {
        for tag in ["Videos", "Audio"] {
            if let Some(url) = crate::upstream::url::replace_path_id(
                operation.original_request.url(),
                tag,
                &member.virtual_media_id,
            ) {
                *operation.original_request.url_mut() = url;
                break;
            }
        }
    }
    Ok(plan.target.backend)
}

async fn forward_media_request(
    state: &PlaybackService<'_>,
    server: &Server,
    request: reqwest::Request,
    log_label: &str,
    proxy_api_key: Option<&str>,
    viewer: Option<&str>,
) -> Result<DeliveryResponse, ApplicationError> {
    let url = request.url().clone();

    match server.media_streaming_mode {
        MediaStreamingMode::Redirect => {
            info!(
                "Redirecting {} via {}: {}",
                log_label,
                server.name,
                url.path()
            );
            let token = JellyfinAuthorization::from_request(&request).and_then(|auth| auth.token());
            delivery::redirect(&request, token.as_deref())
        }
        MediaStreamingMode::Proxy => {
            info!(
                "Proxying {} from {}: {}",
                log_label,
                server.name,
                url.path()
            );
            let timeout = state.config.read().await.timeout;
            let context = delivery::DeliveryContext::new(
                state.streaming_reqwest_client,
                crate::upstream::jellyfin::hls::HlsContext::new(
                    state.processors.url_processor,
                    server,
                    proxy_api_key,
                    viewer,
                    timeout,
                ),
            );
            delivery::proxy(&context, request).await
        }
    }
}

fn proxy_api_key(context: &RequestContext) -> Option<String> {
    // An unrecognized/passthrough backend token is not a proxy credential.
    context.user()?;
    context.authorization()?.token()
}

fn request_user_id(context: &RequestContext) -> Option<&str> {
    context.user().map(|user| user.id.as_str()).or_else(|| {
        context
            .backends()
            .sessions
            .as_ref()
            .and_then(|sessions| sessions.first())
            .map(|(session, _)| session.user_id.as_str())
    })
}

async fn build_request(
    state: &PlaybackService<'_>,
    target: &AuthorizedTarget,
    request: &reqwest::Request,
) -> Result<reqwest::Request, ApplicationError> {
    let prefix = state.get_url_prefix().await;
    UpstreamRequestBuilder::new(state.processors.url_processor, prefix.as_deref())
        .build(target, request)
        .await
        .map_err(|error| ApplicationError::InvalidRouting(error.into()))
}

pub(crate) async fn get_video_resource_typed(
    state: &PlaybackService<'_>,
    mut operation: OperationRequest,
) -> Result<DeliveryResponse, ApplicationError> {
    let resolved = operation
        .resolve_target_using(&state.routing_services())
        .await?;
    let proxy_api_key = proxy_api_key(&operation.context);
    let id = extract_stream_item_id(operation.original_request.url().path())
        .ok_or_else(|| {
            ApplicationError::InvalidInput("missing stream item")
                .at(crate::application::error::FailureStage::ResourceLookup)
        })?
        .to_string();
    let session_id = extract_play_session_id(operation.original_request.url())?;
    let binding = selected_play_session(state, &operation, &id, session_id.as_deref()).await?;
    if binding.is_none() && !resolved.server_matched_request {
        return Err(ApplicationError::MissingIdentity("resource owner")
            .at(crate::application::error::FailureStage::ResourceLookup));
    }
    let target = delivery_target(
        state,
        &mut operation,
        resolved.target,
        &id,
        binding.as_ref(),
        SelectionPolicy::Resource,
    )
    .await?;
    let request = build_request(state, &target, &operation.original_request).await?;
    forward_media_request(
        state,
        target.server(),
        request,
        "media resource",
        proxy_api_key.as_deref(),
        operation
            .context
            .user()
            .filter(|_| proxy_api_key.is_some())
            .map(|user| user.id.as_str()),
    )
    .await
}

pub(crate) async fn get_stream_typed(
    state: &PlaybackService<'_>,
    mut operation: OperationRequest,
) -> Result<DeliveryResponse, ApplicationError> {
    let resolved = operation
        .resolve_target_using(&state.routing_services())
        .await?;
    let proxy_api_key = proxy_api_key(&operation.context);
    let item_id =
        extract_stream_item_id(operation.original_request.url().path()).map(str::to_string);
    let session_id = extract_play_session_id(operation.original_request.url())?;
    let user_id = request_user_id(&operation.context).map(str::to_string);
    let play_session = if let Some(item_id) = item_id.as_deref() {
        selected_play_session(state, &operation, item_id, session_id.as_deref()).await?
    } else {
        None
    };

    let Some(play_session) = play_session else {
        let target = delivery_target(
            state,
            &mut operation,
            resolved.target,
            item_id.as_deref().unwrap_or_default(),
            None,
            SelectionPolicy::Direct,
        )
        .await?;
        let server = target.server().clone();
        let request = build_request(state, &target, &operation.original_request).await?;
        let original_item_id = extract_stream_item_id(request.url().path()).map(str::to_string);
        let response = forward_media_request(
            state,
            &server,
            request,
            "media stream",
            proxy_api_key.as_deref(),
            operation
                .context
                .user()
                .filter(|_| proxy_api_key.is_some())
                .map(|user| user.id.as_str()),
        )
        .await?;
        super::lifecycle::PlaybackLifecycle::new(state.play_sessions)
            .direct_stream(
                response.status,
                session_id.as_deref(),
                item_id.as_deref(),
                user_id.as_deref(),
                server.id,
                original_item_id,
            )
            .await;
        return Ok(response);
    };
    let target = delivery_target(
        state,
        &mut operation,
        resolved.target,
        item_id.as_deref().unwrap_or_default(),
        Some(&play_session),
        SelectionPolicy::Direct,
    )
    .await?;
    let request = build_request(state, &target, &operation.original_request).await?;
    forward_media_request(
        state,
        target.server(),
        request,
        "media stream",
        proxy_api_key.as_deref(),
        operation
            .context
            .user()
            .filter(|_| proxy_api_key.is_some())
            .map(|user| user.id.as_str()),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::servers::id::ServerId;

    async fn direct_fixture(
        mode: MediaStreamingMode,
    ) -> (
        AppState,
        Server,
        RequestContext,
        String,
        wiremock::MockServer,
    ) {
        use crate::{
            application::playback::sessions::SessionStorage,
            auth::service::{LocalCredential, User, UserAuthorizationService},
            config::{AppConfig, MIGRATOR},
            libraries::service::VirtualLibraryService,
            media::storage::MediaStorageService,
            processors::ProxyProcessors,
            servers::storage::ServerStorageService,
            DataContext,
        };
        use std::sync::Arc;
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let upstream = wiremock::MockServer::start().await;
        let servers = ServerStorageService::new(pool.clone());
        let id = servers
            .add_server("Owner", &upstream.uri(), 100, mode)
            .await
            .unwrap();
        let server = servers.get_server_by_id(id).await.unwrap().unwrap();
        wiremock::Mock::given(wiremock::matchers::path("/System/Info/Public"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ServerName":"Owner"})),
            )
            .mount(&upstream)
            .await;
        servers.check_servers_health().await;
        upstream.reset().await;
        let media = MediaStorageService::new(pool.clone());
        let item = media
            .get_or_create_media_mapping("upstream-item", &server)
            .await
            .unwrap();
        let data = DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(servers.clone()),
            media_storage: Arc::new(media.clone()),
            virtual_library_service: Arc::new(VirtualLibraryService::new(pool, servers, media)),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        };
        let state = AppState::new(
            reqwest::Client::new(),
            reqwest::Client::new(),
            data.clone(),
            ProxyProcessors::new(data),
            Default::default(),
        );
        let original = reqwest::Request::new(
            reqwest::Method::GET,
            "http://proxy.example/".parse().unwrap(),
        );
        let context = RequestContext::new(
            &original,
            None,
            Some(User {
                id: "viewer".into(),
                virtual_key: "proxy-token".into(),
                original_username: "Viewer".into(),
                local_credential: LocalCredential::Passwordless,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }),
            None,
            None,
        );
        (state, server, context, item.virtual_media_id, upstream)
    }

    #[tokio::test]
    async fn delivery_transport_retains_cause_without_authenticated_url() {
        let (state, server, _, _, _) = direct_fixture(MediaStreamingMode::Proxy).await;
        let context = delivery::DeliveryContext::new(
            &state.streaming_reqwest_client,
            crate::upstream::jellyfin::hls::HlsContext::new(
                &state.processors.url_processor,
                &server,
                None,
                None,
                5,
            ),
        );
        let request = reqwest::Request::new(
            reqwest::Method::GET,
            "ftp://upstream/Videos/item/stream?ApiKey=private-credential"
                .parse()
                .unwrap(),
        );
        let error = match delivery::proxy(&context, request).await {
            Ok(_) => panic!("unsupported scheme must fail transport"),
            Err(error) => error,
        };
        use std::error::Error;
        assert_eq!(error.detail_status(), StatusCode::BAD_GATEWAY);
        assert!(matches!(error, ApplicationError::UpstreamTransport(_)));
        assert!(error
            .source()
            .unwrap()
            .downcast_ref::<reqwest::Error>()
            .unwrap()
            .url()
            .is_none());
        assert!(!format!("{error:?}").contains("private-credential"));
    }

    #[tokio::test]
    async fn unknown_direct_session_binds_only_after_success() {
        let (state, server, context, item, upstream) =
            direct_fixture(MediaStreamingMode::Proxy).await;
        let operation = |session: &str| OperationRequest {
            original_request: reqwest::Request::new(
                reqwest::Method::GET,
                format!("http://proxy.example/Videos/{item}/stream?PlaySessionId={session}")
                    .parse()
                    .unwrap(),
            ),
            context: RequestContext::new(
                &reqwest::Request::new(
                    reqwest::Method::GET,
                    "http://proxy.example/".parse().unwrap(),
                ),
                None,
                context.user().cloned(),
                None,
                None,
            ),
            body_analysis: None,
        };
        wiremock::Mock::given(wiremock::matchers::path("/Videos/upstream-item/stream"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&upstream)
            .await;
        let failed = get_stream_typed(&state.playback_service(), operation("direct"))
            .await
            .unwrap();
        assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(state
            .play_sessions
            .resolve_report_session("direct", "viewer")
            .await
            .unwrap()
            .is_none());
        upstream.reset().await;
        wiremock::Mock::given(wiremock::matchers::path("/Videos/upstream-item/stream"))
            .respond_with(
                wiremock::ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 0-2/3")
                    .set_body_bytes(b"abc".to_vec()),
            )
            .mount(&upstream)
            .await;
        let delivered = get_stream_typed(&state.playback_service(), operation("direct"))
            .await
            .unwrap();
        assert_eq!(delivered.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            delivered.headers[hyper::header::CONTENT_RANGE],
            "bytes 0-2/3"
        );
        assert!(matches!(delivered.body, delivery::DeliveryBody::Stream(_)));
        let (binding, _) = state
            .play_sessions
            .resolve_report_session("direct", "viewer")
            .await
            .unwrap()
            .unwrap()
            .into_parts();
        assert_eq!(binding.server_id, server.id);
        assert_eq!(binding.item_id, item);
        assert_eq!(binding.original_item_id.as_deref(), Some("upstream-item"));
    }

    #[tokio::test]
    async fn unknown_direct_session_binds_on_redirect() {
        let (state, server, context, item, upstream) =
            direct_fixture(MediaStreamingMode::Redirect).await;
        let response = get_stream_typed(&state.playback_service(), OperationRequest {
            original_request: reqwest::Request::new(reqwest::Method::HEAD,
                format!("http://proxy.example/Audio/{item}/universal?PlaySessionId=redirect&Extension=kept").parse().unwrap()),
            context, body_analysis: None,
        }).await.unwrap();
        assert_eq!(response.status, StatusCode::TEMPORARY_REDIRECT);
        assert!(matches!(response.body, delivery::DeliveryBody::Empty));
        assert!(response.headers[hyper::header::LOCATION]
            .to_str()
            .unwrap()
            .contains("/Audio/upstream-item/universal?PlaySessionId=redirect&Extension=kept"));
        let (binding, _) = state
            .play_sessions
            .resolve_report_session("redirect", "viewer")
            .await
            .unwrap()
            .unwrap()
            .into_parts();
        assert_eq!(binding.server_id, server.id);
        assert_eq!(binding.original_item_id.as_deref(), Some("upstream-item"));
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    fn playback_session(session_id: &str, user_id: &str, server_id: i64) -> PlaybackSession {
        PlaybackSession {
            session_id: session_id.to_string(),
            item_id: "item-1".to_string(),
            user_id: user_id.to_string(),
            server_id: ServerId::new(server_id),
            original_item_id: None,
        }
    }

    #[test]
    fn extract_stream_item_id_from_videos_and_audio() {
        assert_eq!(
            extract_stream_item_id("/Videos/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/master.m3u8"),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
        assert_eq!(
            extract_stream_item_id("/Audio/11111111111111111111111111111111/universal"),
            Some("11111111111111111111111111111111")
        );
        assert_eq!(
            extract_stream_item_id("/Audio/11111111111111111111111111111111/hls1/main/0.ts"),
            Some("11111111111111111111111111111111")
        );
        assert_eq!(extract_stream_item_id("/Items/abc/PlaybackInfo"), None);
    }

    #[test]
    fn matching_play_session_accepts_one_candidate() {
        let session = matching_play_session(
            vec![playback_session("session-1", "user-1", 1)],
            None,
            Some("user-1"),
        )
        .unwrap();

        assert_eq!(session.session_id, "session-1");
        assert_eq!(session.server_id, ServerId::new(1));
    }

    #[test]
    fn matching_play_session_filters_by_user() {
        let session = matching_play_session(
            vec![
                playback_session("session-1", "user-1", 1),
                playback_session("session-2", "user-2", 2),
            ],
            None,
            Some("user-2"),
        )
        .unwrap();

        assert_eq!(session.session_id, "session-2");
        assert_eq!(session.server_id, ServerId::new(2));
    }

    #[test]
    fn matching_play_session_rejects_ambiguous_matches() {
        let result = matching_play_session(
            vec![
                playback_session("session-1", "user-1", 1),
                playback_session("session-2", "user-1", 2),
            ],
            None,
            Some("user-1"),
        );

        assert!(matches!(result, Err(2)));
    }

    #[test]
    fn matching_play_session_rejects_missing_user_match() {
        let result = matching_play_session(
            vec![playback_session("session-1", "user-1", 1)],
            None,
            Some("user-2"),
        );

        assert!(matches!(result, Err(0)));
    }

    #[test]
    fn matching_play_session_does_not_replace_unknown_session() {
        let result = matching_play_session(
            vec![playback_session("session-1", "user-1", 1)],
            Some("unknown"),
            Some("user-1"),
        );

        assert!(matches!(result, Err(0)));
    }
}
