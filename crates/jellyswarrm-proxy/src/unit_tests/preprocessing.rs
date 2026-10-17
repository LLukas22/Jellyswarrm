//! Incoming request identity, body translation, authorization, and playback-report regressions.
use super::*;
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

use crate::application::playback::sessions::{PlaybackSession, SessionStorage};
use crate::auth::service::UserAuthorizationService;
use crate::config::{AppConfig, MIGRATOR};
use crate::http::handlers::quick_connect::QuickConnectStorage;
use crate::libraries::service::VirtualLibraryService;
use crate::media::storage::MediaStorageService;
use crate::servers::id::ServerId;
use crate::servers::storage::ServerStorageService;
use crate::{processors::ProxyProcessors, DataContext};
use sqlx::SqlitePool;
use std::sync::Arc;

async fn create_test_app_state() -> AppState {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let server_storage = ServerStorageService::new(pool.clone());
    let media_storage = MediaStorageService::new(pool.clone());

    let data_context = DataContext {
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

    let (_request, auth, user, sessions, _analysis, _scope) =
        extract_request_infos(request, &state, None).await.unwrap();

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
            let (_, _, user, _, analysis, _) = extract_request_infos(request, &state, Some(action))
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
        let (_, _, _, _, analysis, _) =
            extract_request_infos(request, &state, Some(PlaybackSessionAction::Refresh))
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
            user_id: "victim".to_string(),
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
