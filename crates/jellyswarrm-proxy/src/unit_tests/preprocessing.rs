//! Incoming request identity, body translation, authorization, and playback-report regressions.
use super::*;
use crate::models::Authorization;
use crate::processors::url_processor::{
    matches_case_insensitive, MEDIA_ID_PATH_TAGS, MEDIA_ID_QUERY_TAGS,
};
use crate::upstream::url::contains_id;

#[test]
fn media_id_tags_cover_audio_paths_and_item_id_queries() {
    let audio_id = "11111111111111111111111111111111";
    let audio_url = url::Url::parse(&format!(
        "http://localhost/Audio/{audio_id}/universal?ItemId=22222222222222222222222222222222"
    ))
    .unwrap();

    let matched_path_id = MEDIA_ID_PATH_TAGS
        .iter()
        .find_map(|path_segment| contains_id(&audio_url, path_segment));

    assert_eq!(matched_path_id.as_deref(), Some(audio_id));
    assert!(matches_case_insensitive("ItemId", MEDIA_ID_QUERY_TAGS));
    assert!(matches_case_insensitive("AlbumId", MEDIA_ID_QUERY_TAGS));
    assert!(matches_case_insensitive("AlbumIds", MEDIA_ID_QUERY_TAGS));
    assert!(matches_case_insensitive("ArtistIds", MEDIA_ID_QUERY_TAGS));
    assert!(matches_case_insensitive(
        "ContributingArtistIds",
        MEDIA_ID_QUERY_TAGS
    ));
    assert!(matches_case_insensitive(
        "AlbumArtistIds",
        MEDIA_ID_QUERY_TAGS
    ));
    assert!(matches_case_insensitive(
        "ExcludeArtistIds",
        MEDIA_ID_QUERY_TAGS
    ));
}

use crate::auth::service::UserAuthorizationService;
use crate::http::handlers::quick_connect::QuickConnectStorage;
use crate::libraries::service::VirtualLibraryService;
use crate::media::storage::MediaStorageService;
use crate::playback::sessions::{PlaybackSession, SessionStorage};
use crate::servers::id::ServerId;
use crate::servers::storage::ServerStorageService;
use crate::{config::AppConfig, persistence::MIGRATOR};
use crate::{processors::ProxyProcessors, DataContext};
use sqlx::SqlitePool;
use std::sync::Arc;
use wiremock::{
    matchers::{header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

#[tokio::test]
async fn abbreviated_seasons_headers_dispatch_with_stored_credentials() {
    let state = create_test_app_state().await;
    let caller = test_user(&state, "streamyfin").await;
    let other = test_user(&state, "other").await;
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/System/Info/Public"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&upstream)
        .await;
    let id = state
        .server_storage
        .add_server(
            "Seasons",
            &upstream.uri(),
            100,
            crate::config::MediaStreamingMode::Redirect,
        )
        .await
        .unwrap();
    let server = state
        .server_storage
        .get_server_by_id(id)
        .await
        .unwrap()
        .unwrap();
    let stored = Authorization {
        client: "Streamyfin".into(),
        device: "iPhone".into(),
        device_id: "streamyfin-device".into(),
        version: "0.55.2".into(),
        token: Some("upstream-token".into()),
    };
    state
        .user_authorization
        .add_server_mapping(
            &caller.id,
            &server,
            "upstream-user",
            &"password".into(),
            None,
        )
        .await
        .unwrap();
    state
        .user_authorization
        .store_authorization_session(
            &caller.id,
            &server,
            &stored,
            "upstream-token".into(),
            "upstream-user".into(),
            None,
        )
        .await
        .unwrap();
    state.server_storage.check_servers_health().await;
    assert!(state.server_storage.server_status(id).await.is_healthy());
    let show = state
        .media_storage
        .get_or_create_media_mapping("upstream-show", &server)
        .await
        .unwrap();
    let expected_auth = stored.to_header_value();
    Mock::given(method("GET"))
        .and(path("/Shows/upstream-show/Seasons"))
        .and(move |request: &wiremock::Request| {
            request
                .headers
                .get("authorization")
                .is_some_and(|value| value == expected_auth.as_str())
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"Items": []})))
        .expect(3)
        .mount(&upstream)
        .await;
    let full = Authorization {
        token: Some(caller.virtual_key.clone()),
        ..stored.clone()
    };
    for (name, value) in [
        ("authorization", full.to_header_value()),
        (
            "authorization",
            format!(
                r#"MediaBrowser DeviceId="streamyfin-device", Token="{}""#,
                caller.virtual_key
            ),
        ),
        (
            "x-emby-authorization",
            format!(
                r#"MediaBrowser DeviceId="streamyfin-device", Token="{}""#,
                caller.virtual_key
            ),
        ),
    ] {
        let uri: http::Uri = format!("/Shows/{}/Seasons", show.virtual_media_id)
            .parse()
            .unwrap();
        let request = Request::builder()
            .uri(uri.clone())
            .extension(OriginalUri(uri))
            .header(name, value)
            .body(axum::body::Body::empty())
            .unwrap();
        let response =
            crate::http::proxy::proxy_handler(axum::extract::State(state.clone()), request)
                .await
                .unwrap();
        assert_eq!(
            response.status(),
            200,
            "upstream requests: {:?}",
            upstream.received_requests().await.unwrap()
        );
        // Consume the streaming response; the mock verifies the rewritten URL and stored credentials.
        response.into_body().collect().await.unwrap();
    }
    let local = state.client_sessions.for_user(&caller.id).await;
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].device.client, stored.client);
    assert_eq!(local[0].device.device, stored.device);
    assert_eq!(local[0].device.version, stored.version);

    let mut partial = Device {
        client: String::new(),
        device: String::new(),
        version: String::new(),
        device_id: stored.device_id.clone(),
    };
    assert!(state
        .user_authorization
        .get_user_sessions_for_partial_device(&other.id, Some(&partial))
        .await
        .unwrap()
        .is_empty());
    partial.device_id.push_str("-wrong");
    assert!(state
        .user_authorization
        .get_user_sessions_for_partial_device(&caller.id, Some(&partial))
        .await
        .unwrap()
        .is_empty());
    partial.device_id = stored.device_id.clone();
    partial.client = "Another client".into();
    assert!(state
        .user_authorization
        .get_user_sessions_for_partial_device(&caller.id, Some(&partial))
        .await
        .unwrap()
        .is_empty());
    partial.client = stored.client.clone();
    assert_eq!(
        state
            .user_authorization
            .get_user_sessions_for_partial_device(&caller.id, Some(&partial))
            .await
            .unwrap()
            .len(),
        1
    );
    state
        .user_authorization
        .store_authorization_session(
            &caller.id,
            &server,
            &stored,
            "expired-token".into(),
            "upstream-user".into(),
            Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
        )
        .await
        .unwrap();
    assert!(state
        .user_authorization
        .get_user_sessions_for_partial_device(&caller.id, Some(&partial))
        .await
        .unwrap()
        .is_empty());
}

async fn create_test_app_state() -> AppState {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let server_storage = ServerStorageService::new(pool.clone());
    let media_storage = MediaStorageService::new(pool.clone());

    let data_context = DataContext {
        paths: crate::config::Paths::temporary(),
        user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
        server_storage: Arc::new(server_storage.clone()),
        media_storage: Arc::new(media_storage.clone()),
        virtual_library_service: Arc::new(VirtualLibraryService::new(
            pool,
            server_storage,
            media_storage,
        )),
        play_sessions: Arc::new(SessionStorage::new()),
        config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
    };
    let processors = ProxyProcessors::new(data_context.clone());

    AppState::new(
        reqwest::Client::new(),
        reqwest::Client::new(),
        data_context,
        processors,
        QuickConnectStorage::new(),
    )
}

async fn preprocessed_image_request(state: &AppState, upstream: &MockServer) -> reqwest::Request {
    let server_id = state
        .server_storage
        .add_server(
            "Images",
            &format!("{}/jellyfin", upstream.uri()),
            100,
            crate::config::MediaStreamingMode::Redirect,
        )
        .await
        .unwrap();
    let server = state
        .server_storage
        .get_server_by_id(server_id)
        .await
        .unwrap()
        .unwrap();
    let mut request = reqwest::Client::new()
        .get("http://localhost/Items/poster/Images/Primary?fillWidth=292&tag=image-tag")
        .header(reqwest::header::HOST, "jellyswarrm.example.com")
        .build()
        .unwrap();
    let auth = Some(JellyfinAuthorization::Authorization(Authorization {
        client: "Test".into(),
        device: "Test".into(),
        device_id: "test-device".into(),
        version: "1.0.0".into(),
        token: Some("upstream-token".into()),
    }));

    apply_to_request(&mut request, &server, &None, &auth, state, None).await;

    assert!(!request.headers().contains_key(reqwest::header::HOST));
    assert_eq!(
        request.url().as_str(),
        format!(
            "{}/jellyfin/Items/poster/Images/Primary?fillWidth=292&tag=image-tag",
            upstream.uri()
        )
    );
    request
}

#[tokio::test]
async fn image_request_uses_upstream_host_and_non_default_port() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jellyfin/Items/poster/Images/Primary"))
        .and(header("host", upstream.address().to_string()))
        .and(query_param("fillWidth", "292"))
        .and(query_param("tag", "image-tag"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"poster"))
        .expect(1)
        .mount(&upstream)
        .await;
    let state = create_test_app_state().await;
    let request = preprocessed_image_request(&state, &upstream).await;

    let response = state.reqwest_client.execute(request).await.unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"poster");
}

#[tokio::test]
async fn image_request_follows_cross_origin_redirects_with_destination_host() {
    let upstream = MockServer::start().await;
    let images = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jellyfin/Items/poster/Images/Primary"))
        .and(header("host", upstream.address().to_string()))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/poster.png", images.uri())),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/poster.png"))
        .and(header("host", images.address().to_string()))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", "/original.png"))
        .expect(1)
        .mount(&images)
        .await;
    Mock::given(method("GET"))
        .and(path("/original.png"))
        .and(header("host", images.address().to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"poster"))
        .expect(1)
        .mount(&images)
        .await;
    let state = create_test_app_state().await;
    let request = preprocessed_image_request(&state, &upstream).await;

    let response = state.reqwest_client.execute(request).await.unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.url().as_str(),
        format!("{}/original.png", images.uri())
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"poster");
}

#[tokio::test]
async fn image_request_follows_same_origin_redirects_preserving_authorization() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jellyfin/Items/poster/Images/Primary"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", "/poster.png"))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/poster.png"))
        .and(header("host", upstream.address().to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"poster"))
        .expect(1)
        .mount(&upstream)
        .await;
    let state = create_test_app_state().await;
    let request = preprocessed_image_request(&state, &upstream).await;
    let authorization = request.headers()[reqwest::header::AUTHORIZATION].clone();

    let response = state.reqwest_client.execute(request).await.unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"poster");
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(
            request.headers[reqwest::header::AUTHORIZATION],
            authorization
        );
    }
}

async fn test_user(state: &AppState, name: &str) -> User {
    state
        .user_authorization
        .get_or_create_user(name, &"password123".into())
        .await
        .unwrap()
}

fn json_post(path: &str, token: &str, body: serde_json::Value) -> Request {
    let uri: http::Uri = path.parse().unwrap();
    Request::builder()
        .method(http::Method::POST)
        .uri(uri.clone())
        .header(http::header::CONTENT_TYPE, "application/json")
        .header("X-MediaBrowser-Token", token)
        .extension(OriginalUri(uri))
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn resolve_identity_ignores_valid_userid_path_segment_without_auth() {
    let state = create_test_app_state().await;
    let victim = test_user(&state, "victim").await;

    let headers = http::HeaderMap::new();
    let uri: http::Uri = format!("/Users/{}", victim.id).parse().unwrap();

    let identity = resolve_request_identity_from_headers_uri(&headers, &uri, &state)
        .await
        .unwrap();

    assert!(identity.user.is_none());
}

#[tokio::test]
async fn resolve_identity_ignores_valid_userid_query_on_user_views_without_auth() {
    let state = create_test_app_state().await;
    let victim = test_user(&state, "victim").await;

    let headers = http::HeaderMap::new();
    let uri: http::Uri = format!("/UserViews?userId={}", victim.id).parse().unwrap();

    let identity = resolve_request_identity_from_headers_uri(&headers, &uri, &state)
        .await
        .unwrap();

    assert!(identity.user.is_none());
}

#[tokio::test]
async fn resolve_identity_ignores_valid_userid_query_on_user_items_resume_without_auth() {
    let state = create_test_app_state().await;
    let victim = test_user(&state, "victim").await;

    let headers = http::HeaderMap::new();
    let uri: http::Uri = format!("/UserItems/Resume?userId={}", victim.id)
        .parse()
        .unwrap();

    let identity = resolve_request_identity_from_headers_uri(&headers, &uri, &state)
        .await
        .unwrap();

    assert!(identity.user.is_none());
}

#[tokio::test]
async fn resolve_identity_treats_malformed_authorization_header_as_unauthenticated() {
    let state = create_test_app_state().await;
    let victim = test_user(&state, "victim").await;

    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_static("Bearer sometoken"),
    );
    let uri: http::Uri = format!("/UserViews?userId={}", victim.id).parse().unwrap();

    let identity = resolve_request_identity_from_headers_uri(&headers, &uri, &state)
        .await
        .unwrap();

    assert!(identity.auth.is_none());
    assert!(identity.user.is_none());
}

#[tokio::test]
async fn resolve_identity_resolves_authenticated_user_ignoring_different_userid_in_url() {
    let state = create_test_app_state().await;
    let caller = test_user(&state, "caller").await;
    let other = test_user(&state, "other").await;

    let auth_header = Authorization {
        client: "Test".to_string(),
        device: "Test".to_string(),
        device_id: "test-device".to_string(),
        version: "1.0.0".to_string(),
        token: Some(caller.virtual_key.clone()),
    }
    .to_header_value();

    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&auth_header).unwrap(),
    );
    let uri: http::Uri = format!("/UserViews?userId={}", other.id).parse().unwrap();

    let identity = resolve_request_identity_from_headers_uri(&headers, &uri, &state)
        .await
        .unwrap();

    assert_eq!(identity.user.unwrap().id, caller.id);
}

#[tokio::test]
async fn invalid_token_cannot_adopt_user_from_request_body() {
    let state = create_test_app_state().await;
    let victim = test_user(&state, "victim").await;
    let request = json_post(
        "/Sessions/Capabilities/Full",
        "invalid-token",
        serde_json::json!({ "UserId": victim.id }),
    );

    let crate::application::input::CollectedRequest {
        auth,
        user,
        sessions,
        ..
    } = extract_request_infos(request, &state, None).await.unwrap();

    assert!(auth.is_some());
    assert!(user.is_none());
    assert!(sessions.is_none());
}

async fn playback_server(state: &AppState) -> Server {
    let server_id = state
        .server_storage
        .add_server(
            "Playback",
            "http://playback.example",
            100,
            crate::config::MediaStreamingMode::Redirect,
        )
        .await
        .unwrap();
    state
        .server_storage
        .get_server_by_id(server_id)
        .await
        .unwrap()
        .unwrap()
}

fn authorization_session(caller: &User, server: &Server) -> AuthorizationSession {
    let now = chrono::Utc::now();
    AuthorizationSession {
        id: 1,
        user_id: caller.id.clone(),
        mapping_id: 1,
        server_url: server.url.to_string(),
        device: Device::from_useragent("Test"),
        jellyfin_token: "upstream-token".into(),
        original_user_id: "upstream-user".into(),
        expires_at: None,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn playback_reports_route_without_cached_session_authority() {
    let state = create_test_app_state().await;
    let caller = test_user(&state, "caller").await;
    let server = playback_server(&state).await;
    let item = state
        .media_storage
        .get_or_create_media_mapping("item", &server)
        .await
        .unwrap();
    let source = state
        .media_storage
        .get_or_create_media_mapping("source", &server)
        .await
        .unwrap();
    let sessions = Some(vec![(
        authorization_session(&caller, &server),
        server.clone(),
    )]);

    for (path, action) in [
        ("/Sessions/Playing", PlaybackSessionAction::Start),
        ("/Sessions/Playing/Progress", PlaybackSessionAction::Refresh),
        ("/Sessions/Playing/Stopped", PlaybackSessionAction::Remove),
    ] {
        for (case, session_id, source_id) in [
            ("missing session", None, source.virtual_media_id.as_str()),
            (
                "null session",
                Some(serde_json::Value::Null),
                source.virtual_media_id.as_str(),
            ),
            (
                "unknown session",
                Some(serde_json::json!("unknown-session")),
                source.virtual_media_id.as_str(),
            ),
            (
                "empty session",
                Some(serde_json::json!("")),
                source.virtual_media_id.as_str(),
            ),
            (
                "whitespace session",
                Some(serde_json::json!("  ")),
                source.virtual_media_id.as_str(),
            ),
            ("empty session and source", Some(serde_json::json!("")), ""),
            (
                "empty session and whitespace source",
                Some(serde_json::json!("")),
                "  ",
            ),
        ] {
            let mut body = serde_json::json!({
                "ItemId": item.virtual_media_id,
                "MediaSourceId": source_id,
                "NowPlayingQueue": [{"Id": "unrelated-queue-item"}]
            });
            if let Some(session_id) = session_id {
                body["PlaySessionId"] = session_id;
            }
            let request = json_post(path, &caller.virtual_key, body.clone());
            let crate::application::input::CollectedRequest {
                user,
                body_analysis: analysis,
                ..
            } = extract_request_infos(request, &state, Some(action))
                .await
                .unwrap_or_else(|error| panic!("{path}: {case}: {error}"));
            assert_eq!(user.expect(case).id, caller.id, "{path}: {case}");
            let analysis = analysis.expect(case);
            assert!(
                analysis.authoritative_play_session.is_none(),
                "{path}: {case}"
            );
            if source_id.trim().is_empty() {
                assert!(
                    analysis.requested_play_source_id.is_none(),
                    "{path}: {case}"
                );
            }
            if body
                .get("PlaySessionId")
                .and_then(|id| id.as_str())
                .is_some_and(|id| id.trim().is_empty())
            {
                assert!(
                    analysis.requested_play_session_id.is_none(),
                    "{path}: {case}"
                );
                assert!(analysis.found_session_ids.is_empty(), "{path}: {case}");
            }
            let (selected, auth, matched) =
                resolve_playback_report_server(&sessions, &analysis, &state)
                    .await
                    .unwrap_or_else(|error| panic!("{path}: {case}: {error}"));
            assert_eq!(selected.id, server.id, "{path}: {case}");
            assert_eq!(auth.expect(case).user_id, caller.id, "{path}: {case}");
            assert!(matched, "{path}: {case}");
        }
    }
}

#[tokio::test]
async fn playback_report_rejects_unavailable_or_conflicting_routes() {
    let state = create_test_app_state().await;
    let caller = test_user(&state, "caller").await;
    let server = playback_server(&state).await;
    let item = state
        .media_storage
        .get_or_create_media_mapping("item", &server)
        .await
        .unwrap();
    let sessions = Some(vec![(
        authorization_session(&caller, &server),
        server.clone(),
    )]);
    let analysis = RequestBodyAnalysisResult {
        requested_play_item_id: Some(item.virtual_media_id.clone()),
        ..Default::default()
    };
    assert_eq!(
        resolve_playback_report_server(&sessions, &analysis, &state)
            .await
            .unwrap()
            .0
            .id,
        server.id,
        "item-only route"
    );
    assert!(
        resolve_playback_report_server(&None, &analysis, &state)
            .await
            .is_err(),
        "missing authorization sessions"
    );

    for (case, analysis) in [
        (
            "conflicting session authority",
            RequestBodyAnalysisResult {
                authoritative_play_session: Some(
                    PlaybackSession {
                        session_id: "known-session".into(),
                        item_id: item.virtual_media_id.clone(),
                        user_id: caller.id.clone(),
                        server_id: ServerId::new(server.id.as_i64() + 1),
                        original_item_id: None,
                    }
                    .into(),
                ),
                requested_play_item_id: Some(item.virtual_media_id.clone()),
                ..Default::default()
            },
        ),
        (
            "unknown source",
            RequestBodyAnalysisResult {
                requested_play_source_id: Some("unknown-source".into()),
                requested_play_item_id: Some(item.virtual_media_id.clone()),
                ..Default::default()
            },
        ),
        (
            "unknown item",
            RequestBodyAnalysisResult {
                requested_play_item_id: Some("unknown-item".into()),
                ..analysis
            },
        ),
    ] {
        assert!(
            resolve_playback_report_server(&sessions, &analysis, &state)
                .await
                .is_err(),
            "{case}"
        );
    }
}

#[tokio::test]
async fn playback_report_rejects_ambiguous_aggregate_without_session_or_source() {
    use crate::media::identity::{MediaAlias, MediaKind, MediaObservation, MediaProvider};
    use crate::media::storage::{MediaCatalogSnapshot, MediaVersionSourceObservation};
    use std::collections::BTreeSet;

    let state = create_test_app_state().await;
    let mut snapshots = Vec::new();
    let mut sessions = Vec::new();
    let mut mappings = Vec::new();
    for name in ["first", "second"] {
        let id = state
            .server_storage
            .add_server(
                name,
                &format!("http://{name}.example"),
                100,
                crate::config::MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = state
            .server_storage
            .get_server_by_id(id)
            .await
            .unwrap()
            .unwrap();
        let mapping = state
            .media_storage
            .get_or_create_media_mapping("item", &server)
            .await
            .unwrap();
        snapshots.push(MediaCatalogSnapshot {
            source_key: name.into(),
            server_id: id,
            complete: true,
            observations: vec![MediaObservation {
                virtual_media_id: mapping.virtual_media_id.clone(),
                kind: MediaKind::Movie,
                aliases: BTreeSet::from([MediaAlias {
                    provider: MediaProvider::Tmdb,
                    kind: MediaKind::Movie,
                    provider_id: "42".into(),
                }]),
            }],
        });
        mappings.push(mapping);
        let now = chrono::Utc::now();
        sessions.push((
            AuthorizationSession {
                id: id.as_i64(),
                user_id: "caller".into(),
                mapping_id: id.as_i64(),
                server_url: server.url.to_string(),
                device: Device::from_useragent("Test"),
                jellyfin_token: "upstream-token".into(),
                original_user_id: "upstream-user".into(),
                expires_at: None,
                created_at: now,
                updated_at: now,
            },
            server,
        ));
    }
    let generation = state
        .media_storage
        .begin_media_reconciliation()
        .await
        .unwrap();
    let groups = state
        .media_storage
        .reconcile_media_catalog("configured:library:caller", generation, &snapshots, true)
        .await
        .unwrap();
    let aggregate_id = groups[&mappings[0].virtual_media_id]
        .virtual_media_id
        .clone();
    let group = state
        .media_storage
        .get_media_version_group(&aggregate_id)
        .await
        .unwrap()
        .unwrap();
    let source = state
        .media_storage
        .get_or_create_media_mapping("source", &sessions[1].1)
        .await
        .unwrap();
    state
        .media_storage
        .replace_media_version_sources(
            group.id,
            generation,
            &[mappings[1].id],
            &[MediaVersionSourceObservation {
                member_mapping_id: mappings[1].id,
                source_virtual_id: source.virtual_media_id.clone(),
            }],
        )
        .await
        .unwrap();
    let sessions = Some(sessions);
    let mut analysis = RequestBodyAnalysisResult {
        requested_play_item_id: Some(aggregate_id),
        ..Default::default()
    };
    assert!(resolve_playback_report_server(&sessions, &analysis, &state)
        .await
        .is_err());
    analysis.requested_play_source_id = Some(source.virtual_media_id.clone());
    assert_eq!(
        resolve_playback_report_server(&sessions, &analysis, &state)
            .await
            .unwrap()
            .0
            .id,
        mappings[1].server_id
    );
    analysis.requested_play_item_id = Some(mappings[0].virtual_media_id.clone());
    assert!(resolve_playback_report_server(&sessions, &analysis, &state)
        .await
        .is_err());
}

#[tokio::test]
async fn playback_reports_reject_missing_auth_and_conflicting_fields() {
    let state = create_test_app_state().await;
    let caller = test_user(&state, "caller").await;
    for (token, body) in [
        (
            "invalid",
            serde_json::json!({"ItemId": "item", "UserId": caller.id}),
        ),
        (
            caller.virtual_key.as_str(),
            serde_json::json!({"ItemId": "item", "itemId": "different"}),
        ),
        (
            caller.virtual_key.as_str(),
            serde_json::json!({"ItemId": "item", "PlaySessionId": "one", "playSessionId": "two"}),
        ),
        (
            caller.virtual_key.as_str(),
            serde_json::json!({"ItemId": "item", "MediaSourceId": "one", "mediaSourceId": "two"}),
        ),
        (
            caller.virtual_key.as_str(),
            serde_json::json!({"NowPlayingQueue": [{"ItemId": "item"}]}),
        ),
        (
            caller.virtual_key.as_str(),
            serde_json::json!({"ItemId": "", "PlaySessionId": ""}),
        ),
        (
            caller.virtual_key.as_str(),
            serde_json::json!({"ItemId": "item", "PlaySessionId": 42}),
        ),
        (
            caller.virtual_key.as_str(),
            serde_json::json!({"ItemId": "item", "MediaSourceId": false}),
        ),
    ] {
        let request = json_post("/Sessions/Playing/Progress", token, body);
        assert!(
            extract_request_infos(request, &state, Some(PlaybackSessionAction::Refresh))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn blank_session_alias_does_not_clear_known_authority() {
    let state = create_test_app_state().await;
    let caller = test_user(&state, "caller").await;
    state
        .play_sessions
        .add_session(PlaybackSession {
            session_id: "known-session".into(),
            item_id: "item".into(),
            user_id: caller.id.clone(),
            server_id: ServerId::new(1),
            original_item_id: None,
        })
        .await;
    for (pascal, camel) in [("known-session", ""), ("", "known-session")] {
        let request = json_post(
            "/Sessions/Playing/Progress",
            &caller.virtual_key,
            serde_json::json!({
                "ItemId": "item", "PlaySessionId": pascal, "playSessionId": camel
            }),
        );
        let crate::application::input::CollectedRequest {
            body_analysis: analysis,
            ..
        } = extract_request_infos(request, &state, Some(PlaybackSessionAction::Refresh))
            .await
            .unwrap();
        let analysis = analysis.unwrap();
        assert_eq!(
            analysis.requested_play_session_id.as_deref(),
            Some("known-session")
        );
        assert_eq!(
            analysis
                .authoritative_play_session
                .as_ref()
                .unwrap()
                .session()
                .session_id,
            "known-session"
        );
        assert!(analysis.authoritative_play_session.is_some());
    }
}

#[tokio::test]
async fn progress_rejects_a_session_owned_by_another_user() {
    let state = create_test_app_state().await;
    let caller = test_user(&state, "caller").await;
    state
        .play_sessions
        .add_session(PlaybackSession {
            session_id: "victim-session".to_string(),
            item_id: "item".to_string(),
            user_id: "victim".into(),
            server_id: ServerId::new(1),
            original_item_id: None,
        })
        .await;
    let request = json_post(
        "/Sessions/Playing/Progress",
        &caller.virtual_key,
        serde_json::json!({
            "PlaySessionId": "victim-session",
            "ItemId": "item"
        }),
    );

    let result = extract_request_infos(request, &state, Some(PlaybackSessionAction::Refresh)).await;

    assert!(result.is_err());
}
