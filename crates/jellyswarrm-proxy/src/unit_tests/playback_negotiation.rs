//! Regression coverage for playback negotiation, source reconciliation, and binding commits.
use std::{collections::BTreeSet, sync::Arc};

use super::*;
use crate::{
    application::context::RequestContext,
    application::playback::sessions::SessionStorage,
    auth::service::{AuthorizationSession, Device, UserAuthorizationService},
    config::{AppConfig, MediaStreamingMode, MIGRATOR},
    http::handlers::quick_connect::QuickConnectStorage,
    http::preprocessing::JellyfinAuthorization,
    libraries::service::{VirtualLibraryAccessScope, VirtualLibraryService},
    media::identity::{MediaAlias, MediaKind, MediaObservation, MediaProvider},
    media::storage::{MediaCatalogSnapshot, MediaStorageService, MediaVersionSourceObservation},
    processors::ProxyProcessors,
    servers::storage::{Server, ServerStorageService},
    DataContext,
};
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

struct Fixture {
    pool: sqlx::SqlitePool,
    state: AppState,
    upstreams: Vec<MockServer>,
    sessions: Vec<(AuthorizationSession, Server)>,
    aggregate: String,
    sources: Vec<String>,
}

#[tokio::test]
async fn target_bound_translation_does_not_repeat_backend_availability_selection() {
    let fixture = fixture().await;
    let members = fixture
        .state
        .media_storage
        .get_media_version_members_by_virtual_id(&fixture.aggregate)
        .await
        .unwrap();
    let member = &members[0];
    // A fresh registry has no successful health observations. Unbound selection
    // must reject this aggregate, but translating an already selected owner
    // remains stable and never silently switches to another copy.
    let registry = Arc::new(ServerStorageService::new(fixture.pool.clone()));
    let selection = crate::application::url_selection::UrlSelection::new(
        fixture.state.media_storage.clone(),
        registry,
        fixture.state.virtual_library_service.clone(),
    );
    let mut url: url::Url = format!("http://proxy/Items/{}", fixture.aggregate)
        .parse()
        .unwrap();
    assert!(selection.server_from_client_url(&url, None).await.is_err());
    fixture
        .state
        .processors
        .url_processor
        .client_to_server_url(&mut url, &None, None, member.server.id)
        .await;
    let expected = members
        .iter()
        .filter(|candidate| candidate.server.id == member.server.id)
        .map(|candidate| candidate.mapping.original_media_id.as_str())
        .max()
        .unwrap();
    assert_eq!(url.path(), format!("/Items/{expected}"));
}

async fn fixture() -> Fixture {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let servers = ServerStorageService::new(pool.clone());
    let media = MediaStorageService::new(pool.clone());
    let context = DataContext {
        user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
        server_storage: Arc::new(servers.clone()),
        media_storage: Arc::new(media.clone()),
        virtual_library_service: Arc::new(VirtualLibraryService::new(pool.clone(), servers, media)),
        play_sessions: Arc::new(SessionStorage::new()),
        config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
    };
    let state = AppState::new(
        reqwest::Client::new(),
        reqwest::Client::new(),
        context.clone(),
        ProxyProcessors::new(context),
        QuickConnectStorage::new(),
    );
    let mut upstreams = Vec::new();
    let mut sessions = Vec::new();
    let mut snapshots = Vec::new();
    let mut owners = Vec::new();
    let mut sources = Vec::new();
    for index in 0..2 {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/System/Info/Public"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ServerName":"Backend"})))
            .mount(&upstream)
            .await;
        let id = state
            .server_storage
            .add_server(
                &format!("Backend {index}"),
                &upstream.uri(),
                100 - index as i32,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = state
            .server_storage
            .get_server_by_id(id)
            .await
            .unwrap()
            .unwrap();
        let now = chrono::Utc::now();
        sessions.push((
            AuthorizationSession {
                id: index,
                user_id: "viewer".into(),
                mapping_id: index,
                server_url: upstream.uri(),
                device: Device {
                    client: "test".into(),
                    device: "test".into(),
                    device_id: "device".into(),
                    version: "1".into(),
                },
                jellyfin_token: format!("backend-token-{index}"),
                original_user_id: format!("backend-viewer-{index}"),
                expires_at: None,
                created_at: now,
                updated_at: now,
            },
            server.clone(),
        ));
        let mut observations = Vec::new();
        // Numbered episodes support multiple versions on one backend. Duplicate
        // movies are deliberately ambiguous and would not form this group.
        for number in if index == 0 {
            vec![100, 101]
        } else {
            vec![102]
        } {
            let owner = state
                .media_storage
                .get_or_create_media_mapping(&format!("{number:032x}"), &server)
                .await
                .unwrap();
            let source = state
                .media_storage
                .get_or_create_media_mapping(&format!("{:032x}", number + 1000), &server)
                .await
                .unwrap();
            observations.push(MediaObservation {
                virtual_media_id: owner.virtual_media_id.clone(),
                kind: MediaKind::Episode,
                aliases: BTreeSet::from([MediaAlias {
                    provider: MediaProvider::SeriesGroup,
                    kind: MediaKind::Episode,
                    provider_id: "matched-series:season:1:episode:1".into(),
                }]),
            });
            owners.push(owner.virtual_media_id);
            sources.push(source.virtual_media_id);
        }
        snapshots.push(MediaCatalogSnapshot {
            source_key: upstream.uri(),
            server_id: server.id,
            complete: true,
            observations,
        });
        upstreams.push(upstream);
    }
    state.server_storage.check_servers_health().await;
    let generation = state
        .media_storage
        .begin_media_reconciliation()
        .await
        .unwrap();
    let assignments = state
        .media_storage
        .reconcile_media_catalog("automatic:library:viewer", generation, &snapshots, true)
        .await
        .unwrap();
    let aggregate = assignments[&owners[0]].virtual_media_id.clone();
    let group = state
        .media_storage
        .get_media_version_group(&aggregate)
        .await
        .unwrap()
        .unwrap();
    let members = state
        .media_storage
        .get_media_version_members(group.id)
        .await
        .unwrap();
    let observations = owners
        .iter()
        .zip(&sources)
        .map(|(owner, source)| MediaVersionSourceObservation {
            member_mapping_id: members
                .iter()
                .find(|member| &member.mapping.virtual_media_id == owner)
                .unwrap()
                .mapping
                .id,
            source_virtual_id: source.clone(),
        })
        .collect::<Vec<_>>();
    state
        .media_storage
        .observe_media_version_sources(
            group.id,
            generation,
            &members
                .iter()
                .map(|member| member.mapping.id)
                .collect::<Vec<_>>(),
            &observations,
        )
        .await
        .unwrap();
    Fixture {
        pool,
        state,
        upstreams,
        sessions,
        aggregate,
        sources,
    }
}

fn operation(fixture: &Fixture, query: &str, payload: serde_json::Value) -> OperationRequest {
    let original_request = reqwest::Client::new()
        .post(format!(
            "http://proxy/Items/{}/PlaybackInfo{query}",
            fixture.aggregate,
        ))
        .json(&payload)
        .build()
        .unwrap();
    let context = RequestContext::new(
        &original_request,
        Some(JellyfinAuthorization::XEmbyToken("proxy-token".into())),
        None,
        Some(fixture.sessions.clone()),
        Some(VirtualLibraryAccessScope::new(
            "viewer",
            fixture.sessions.iter().map(|(_, server)| server.id),
        )),
    );
    OperationRequest {
        original_request,
        context,
        body_analysis: None,
    }
}

#[tokio::test]
async fn first_negotiation_selects_exact_member_before_binding() {
    for selected in [1, 2] {
        let fixture = fixture().await;
        let backend = usize::from(selected == 2);
        let owner = format!("{:032x}", 100 + selected);
        Mock::given(method("POST"))
            .and(path(format!("/Items/{owner}/PlaybackInfo")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "PlaySessionId":"first-play", "MediaSources":[{
                    "Id": format!("{:032x}", 1100 + selected), "Type":"Default"
                }]
            })))
            .expect(1)
            .mount(&fixture.upstreams[backend])
            .await;
        let input = operation(
            &fixture,
            "",
            json!({
                "MediaSourceId":fixture.sources[selected], "UserId":"viewer",
                "AudioStreamIndex":3, "SubtitleStreamIndex":5,
            }),
        );
        assert!(fixture
            .state
            .play_sessions
            .get_session_for_user("first-play", "viewer")
            .await
            .is_none());
        let response = post_playback_info_typed(&fixture.state.playback_service(), input)
            .await
            .unwrap();
        assert_eq!(response.media_sources[0].id, fixture.sources[selected]);
        let requests = fixture.upstreams[backend]
            .received_requests()
            .await
            .unwrap();
        let request = requests
            .iter()
            .find(|request| request.method == "POST")
            .unwrap();
        let body: serde_json::Value = request.body_json().unwrap();
        assert_eq!(body["MediaSourceId"], format!("{:032x}", 1100 + selected));
        assert_eq!(body["UserId"], format!("backend-viewer-{backend}"));
        assert_eq!(body["AudioStreamIndex"], 3);
        assert_eq!(body["SubtitleStreamIndex"], 5);
        assert!(request.headers["authorization"]
            .to_str()
            .unwrap()
            .contains(&format!("backend-token-{backend}")));
        let binding = fixture
            .state
            .play_sessions
            .get_session_for_user("first-play", "viewer")
            .await
            .unwrap();
        assert_eq!(binding.server_id, fixture.sessions[backend].1.id);
        assert_eq!(binding.original_item_id.as_deref(), Some(owner.as_str()));
    }
}

#[test]
fn source_query_precedes_body_including_empty_query() {
    let payload: PlaybackRequest = serde_json::from_value(json!({"MediaSourceId":"body"})).unwrap();
    for (query, expected) in [
        ("", "body"),
        ("?mediaSourceId=query", "query"),
        ("?MediaSourceId=", ""),
    ] {
        let request = reqwest::Request::new(
            reqwest::Method::POST,
            format!("http://proxy/Items/item/PlaybackInfo{query}")
                .parse()
                .unwrap(),
        );
        assert_eq!(
            selected_source(&request, &payload).as_deref(),
            Some(expected)
        );
    }
}

#[tokio::test]
async fn query_source_selects_owner_instead_of_conflicting_body_source() {
    let fixture = fixture().await;
    Mock::given(method("POST"))
        .and(path(format!("/Items/{:032x}/PlaybackInfo", 102)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "PlaySessionId":"query-play", "MediaSources":[{"Id":format!("{:032x}", 1102), "Type":"Default"}]
        })))
        .expect(1).mount(&fixture.upstreams[1]).await;
    let input = operation(
        &fixture,
        &format!("?MediaSourceId={}", fixture.sources[2]),
        json!({"MediaSourceId":fixture.sources[0], "AudioStreamIndex":3}),
    );
    post_playback_info_typed(&fixture.state.playback_service(), input)
        .await
        .unwrap();
    let requests = fixture.upstreams[1].received_requests().await.unwrap();
    let request = requests
        .iter()
        .find(|request| request.method == "POST")
        .unwrap();
    assert_eq!(
        request
            .url
            .query_pairs()
            .find(|(key, _)| key == "MediaSourceId")
            .unwrap()
            .1,
        format!("{:032x}", 1102)
    );
    assert_eq!(
        request.body_json::<serde_json::Value>().unwrap()["AudioStreamIndex"],
        3
    );
    assert!(!fixture.upstreams[0]
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|request| request.method == "POST"));
}

#[tokio::test]
async fn aggregate_default_query_keeps_member_source_and_track_preferences() {
    let fixture = fixture().await;
    // Equal-priority copies on one backend use the greatest original item ID.
    Mock::given(method("POST"))
        .and(path(format!("/Items/{:032x}/PlaybackInfo", 101)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "PlaySessionId":"default-play", "MediaSources":[
                {"Id":format!("{:032x}", 9999), "Type":"Default"},
                {"Id":format!("{:032x}", 101), "Type":"Default", "DefaultAudioStreamIndex":3}
            ]
        })))
        .expect(1)
        .mount(&fixture.upstreams[0])
        .await;
    let input = operation(
        &fixture,
        &format!("?MediaSourceId={}", fixture.aggregate),
        json!({"MediaSourceId":fixture.aggregate, "AudioStreamIndex":3, "SubtitleStreamIndex":5}),
    );
    let response = post_playback_info_typed(&fixture.state.playback_service(), input)
        .await
        .unwrap();
    assert_eq!(response.media_sources.len(), 1);
    assert_eq!(
        response.media_sources[0].default_audio_stream_index,
        Some(3)
    );
    let requests = fixture.upstreams[0].received_requests().await.unwrap();
    let request = requests
        .iter()
        .find(|request| request.method == "POST")
        .unwrap();
    assert!(!request
        .url
        .query_pairs()
        .any(|(key, _)| key.eq_ignore_ascii_case("MediaSourceId")));
    let body: serde_json::Value = request.body_json().unwrap();
    assert!(body["MediaSourceId"].is_null());
    assert_eq!(body["AudioStreamIndex"], 3);
    assert_eq!(body["SubtitleStreamIndex"], 5);
}

#[tokio::test]
async fn sessionless_passthrough_is_rejected_without_negotiation() {
    let fixture = fixture().await;
    let mut input = operation(&fixture, "", json!({}));
    input.context = RequestContext::new(
        &input.original_request,
        input.context.authorization().cloned(),
        input.context.user().cloned(),
        None,
        None,
    );
    assert_eq!(
        post_playback_info_typed(&fixture.state.playback_service(), input)
            .await
            .unwrap_err()
            .detail_status(),
        StatusCode::UNAUTHORIZED
    );
    for upstream in &fixture.upstreams {
        assert!(!upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.method == "POST"));
    }
}

#[tokio::test]
async fn selection_plan_distinguishes_invalid_source_and_unavailable_owner() {
    let fixture = fixture().await;
    let mut input = operation(&fixture, "", json!({}));
    assert!(matches!(
        resolve_playback_route(
            &fixture.state,
            &input.context,
            &fixture.aggregate,
            Some("unknown")
        )
        .await
        .unwrap(),
        PlaybackRouteDecision::InvalidSelectedSource
    ));
    input.context = RequestContext::new(
        &input.original_request,
        input.context.authorization().cloned(),
        input.context.user().cloned(),
        Some(fixture.sessions[..1].to_vec()),
        input.context.access_scope().clone(),
    );
    assert!(matches!(
        resolve_playback_route(
            &fixture.state,
            &input.context,
            &fixture.aggregate,
            Some(&fixture.sources[2])
        )
        .await
        .unwrap(),
        PlaybackRouteDecision::SelectedSourceUnavailable
    ));
    let scope = Some(VirtualLibraryAccessScope::new(
        "viewer",
        [fixture.sessions[0].1.id],
    ));
    input.context = RequestContext::new(
        &input.original_request,
        input.context.authorization().cloned(),
        input.context.user().cloned(),
        input.context.backends().sessions.clone(),
        scope,
    );
    // Denial wins over missing availability, without falling back to a sibling.
    assert!(matches!(
        resolve_playback_route(
            &fixture.state,
            &input.context,
            &fixture.aggregate,
            Some(&fixture.sources[2])
        )
        .await
        .unwrap(),
        PlaybackRouteDecision::InvalidSelectedSource
    ));
    for upstream in &fixture.upstreams {
        assert!(!upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.method == "POST"));
    }
}

#[tokio::test]
async fn failed_negotiation_does_not_commit_playback_binding() {
    let fixture = fixture().await;
    Mock::given(method("POST"))
        .and(path(format!("/Items/{:032x}/PlaybackInfo", 102)))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&fixture.upstreams[1])
        .await;
    let input = operation(&fixture, "", json!({"MediaSourceId":fixture.sources[2]}));
    assert_eq!(
        post_playback_info_typed(&fixture.state.playback_service(), input)
            .await
            .unwrap_err()
            .detail_status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(fixture
        .state
        .play_sessions
        .get_sessions_by_item_id(&fixture.aggregate)
        .await
        .is_empty());
}

#[tokio::test]
async fn shared_resolution_keeps_implicit_ambiguity_and_explicit_binding_conflicts_distinct() {
    use crate::application::playback::sessions::PlaybackSession;
    use crate::application::playback::target::SelectionReason;
    let fixture = fixture().await;
    let input = operation(&fixture, "", json!({}));
    let resolver = MediaTargetResolver {
        media: &fixture.state.media_storage,
    };
    for (index, original) in [(0, 100), (1, 102)] {
        fixture
            .state
            .play_sessions
            .add_session(PlaybackSession {
                session_id: format!("player-{index}"),
                item_id: fixture.aggregate.clone(),
                user_id: "viewer".into(),
                server_id: fixture.sessions[index].1.id,
                original_item_id: Some(format!("{original:032x}")),
            })
            .await;
    }
    // Ambiguity means mapping fallback for delivery, not a universal 400.
    assert!(resolver
        .session(
            &fixture.state.play_sessions,
            &fixture.aggregate,
            None,
            "viewer",
            false
        )
        .await
        .unwrap()
        .is_none());
    // Even a single implicit player cannot override an explicit source.
    assert!(resolver
        .session(
            &fixture.state.play_sessions,
            &fixture.aggregate,
            None,
            "viewer",
            true
        )
        .await
        .unwrap()
        .is_none());
    let binding = resolver
        .session(
            &fixture.state.play_sessions,
            &fixture.aggregate,
            Some("player-0"),
            "viewer",
            true,
        )
        .await
        .unwrap()
        .unwrap();
    // Two versions on the same server still have distinct item authority.
    let conflict = resolver
        .plan(
            &input.context,
            input.context.target(fixture.sessions[0].1.id).unwrap(),
            PlaybackIntent {
                item: &fixture.aggregate,
                source: Some(&fixture.sources[1]),
                binding: Some(&binding),
            },
            &fixture.state.server_storage,
            SelectionPolicy::Direct,
        )
        .await;
    assert!(matches!(conflict, Err(ResolutionError::BindingConflict)));
    let plan = resolver
        .plan(
            &input.context,
            input.context.target(fixture.sessions[0].1.id).unwrap(),
            PlaybackIntent {
                item: &fixture.aggregate,
                source: None,
                binding: Some(&binding),
            },
            &fixture.state.server_storage,
            SelectionPolicy::Resource,
        )
        .await
        .unwrap();
    assert!(matches!(plan.reason, SelectionReason::KnownBinding));
    assert_eq!(
        plan.target.member.unwrap().original_media_id,
        format!("{:032x}", 100)
    );
    assert!(matches!(
        resolver
            .session(
                &fixture.state.play_sessions,
                "another-item",
                Some("player-0"),
                "viewer",
                false
            )
            .await,
        Err(ResolutionError::BindingConflict)
    ));
    assert!(matches!(
        resolver
            .session(
                &fixture.state.play_sessions,
                &fixture.aggregate,
                Some("player-0"),
                "another-viewer",
                false
            )
            .await,
        Err(ResolutionError::ViewerMismatch(_))
    ));
}

#[tokio::test]
async fn shared_explicit_source_plan_never_falls_back_from_offline_owner() {
    let fixture = fixture().await;
    fixture.upstreams[1].reset().await;
    fixture.state.server_storage.check_servers_health().await;
    let input = operation(&fixture, "", json!({}));
    let resolver = MediaTargetResolver {
        media: &fixture.state.media_storage,
    };
    for policy in [
        SelectionPolicy::Negotiation,
        SelectionPolicy::Direct,
        SelectionPolicy::Resource,
    ] {
        let plan = resolver
            .plan(
                &input.context,
                input.context.target(fixture.sessions[0].1.id).unwrap(),
                PlaybackIntent {
                    item: &fixture.aggregate,
                    source: Some(&fixture.sources[2]),
                    binding: None,
                },
                &fixture.state.server_storage,
                policy,
            )
            .await;
        assert!(matches!(plan, Err(ResolutionError::OfflineOwner)));
    }
    assert!(fixture
        .state
        .play_sessions
        .sessions_for_user("viewer")
        .await
        .is_empty());
    for upstream in &fixture.upstreams {
        assert!(!upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.method != "GET" || request.url.path() != "/System/Info/Public"));
    }
}

#[tokio::test]
async fn invalid_live_open_and_failed_refresh_do_not_commit_playback_transitions() {
    use crate::application::playback::sessions::PlaybackSession;
    use crate::application::playback::{
        lifecycle::PlaybackLifecycle,
        live::{self, LiveRequest},
    };
    let fixture = fixture().await;
    let owner = &fixture.sessions[0].1;
    PlaybackLifecycle::new(&fixture.state.play_sessions)
        .establish(PlaybackSession {
            session_id: "live-player".into(),
            item_id: fixture.aggregate.clone(),
            user_id: "viewer".into(),
            server_id: owner.id,
            original_item_id: Some(format!("{:032x}", 100)),
        })
        .await;
    let (_, revision) = fixture
        .state
        .play_sessions
        .resolve_report_session("live-player", "viewer")
        .await
        .unwrap()
        .unwrap()
        .into_parts();
    Mock::given(method("POST"))
        .and(path("/LiveStreams/Open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"MediaSource":false})))
        .expect(1)
        .mount(&fixture.upstreams[0])
        .await;
    let input = operation(&fixture, "", json!({}));
    let request = reqwest::Client::new()
        .post("http://proxy/LiveStreams/Open")
        .json(&json!({"ItemId":fixture.aggregate, "PlaySessionId":"live-player"}))
        .build()
        .unwrap();
    assert_eq!(
        live::post_livestream_open_typed(
            &fixture.state.playback_service(),
            LiveRequest {
                original_request: request,
                context: input.context,
            }
        )
        .await
        .unwrap_err()
        .detail_status(),
        StatusCode::BAD_GATEWAY
    );
    let lifecycle = PlaybackLifecycle::new(&fixture.state.play_sessions);
    let live_id = lifecycle
        .register_live("backend-live", "viewer", owner.id)
        .await;
    Mock::given(method("POST"))
        .and(path("/LiveStreams/Refresh"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&fixture.upstreams[0])
        .await;
    let input = operation(&fixture, "", json!({}));
    let request = reqwest::Client::new()
        .post("http://proxy/LiveStreams/Refresh")
        .json(&json!({"LiveStreamId":live_id}))
        .build()
        .unwrap();
    assert!(matches!(
        live::post_live_stream_action_typed(
            &fixture.state.playback_service(),
            LiveRequest {
                original_request: request,
                context: input.context,
            }
        )
        .await
        .map_err(|error| error.detail_status()),
        Err(StatusCode::SERVICE_UNAVAILABLE)
    ));
    let binding = fixture
        .state
        .play_sessions
        .live_stream(&live_id, "viewer")
        .await
        .unwrap();
    assert_eq!(binding.original_id.as_str(), "backend-live");
    let (binding, current_revision) = fixture
        .state
        .play_sessions
        .resolve_report_session("live-player", "viewer")
        .await
        .unwrap()
        .unwrap()
        .into_parts();
    assert_eq!(current_revision, revision);
    assert_eq!(
        binding.original_item_id.as_deref(),
        Some(format!("{:032x}", 100).as_str())
    );
}

async fn live_open_input(
    fixture: &Fixture,
    original: &str,
) -> crate::application::playback::live::LiveRequest {
    fixture
        .state
        .play_sessions
        .add_session(crate::application::playback::sessions::PlaybackSession {
            session_id: "live-player".into(),
            item_id: fixture.aggregate.clone(),
            user_id: "viewer".into(),
            server_id: fixture.sessions[0].1.id,
            original_item_id: Some(original.into()),
        })
        .await;
    crate::application::playback::live::LiveRequest {
        original_request: reqwest::Client::new().post("http://proxy/LiveStreams/Open")
            .json(&json!({"ItemId":fixture.aggregate,"PlaySessionId":"live-player","UserId":"viewer","OpenToken":"opaque-token"}))
            .build().unwrap(),
        context: operation(fixture, "", json!({})).context,
    }
}

#[tokio::test]
async fn valid_live_sources_do_not_publish_when_translation_or_reconciliation_fails() {
    for open in [false, true] {
        for translation in [false, true] {
            for existing in [0, 1, 2] {
                let fixture = fixture().await;
                let server = &fixture.sessions[0].1;
                let original = format!("{:032x}", 100);
                let input = if open {
                    Some(live_open_input(&fixture, &original).await)
                } else {
                    None
                };
                let before = fixture
                    .state
                    .play_sessions
                    .resolve_report_session("live-player", "viewer")
                    .await
                    .unwrap();
                let abandoned = if existing == 2 {
                    let old = fixture
                        .state
                        .play_sessions
                        .register_live_stream("backend-live", "viewer", server.id)
                        .await;
                    let pending = fixture
                        .state
                        .play_sessions
                        .prepare_live_stream("backend-live", "viewer", server.id)
                        .await;
                    fixture
                        .state
                        .play_sessions
                        .remove_live_stream(&old, "viewer")
                        .await;
                    Some(pending)
                } else {
                    None
                };
                let existing_id = if existing > 0 {
                    Some(
                        fixture
                            .state
                            .play_sessions
                            .register_live_stream("backend-live", "viewer", server.id)
                            .await,
                    )
                } else {
                    None
                };
                let plan = fixture
                    .state
                    .play_sessions
                    .prepare_live_stream("backend-live", "viewer", server.id)
                    .await;
                let other_plan = fixture
                    .state
                    .play_sessions
                    .prepare_live_stream("other-live", "viewer", server.id)
                    .await;
                let source_id = "deadbeefdeadbeefdeadbeefdeadbeef";
                let trigger = if translation {
                    format!("CREATE TRIGGER fail_translation BEFORE INSERT ON media_mappings WHEN NEW.original_media_id = '{source_id}' BEGIN SELECT RAISE(ABORT, 'translation failure'); END")
                } else {
                    "CREATE TRIGGER fail_reconciliation BEFORE UPDATE OF sources_generation ON movie_version_members BEGIN SELECT RAISE(ABORT, 'reconciliation failure'); END".into()
                };
                sqlx::query(&trigger).execute(&fixture.pool).await.unwrap();
                let source = json!({"Id":source_id,"LiveStreamId":"backend-live", "StreamUrl":format!("/Videos/{original}/stream?LiveStreamId=backend-live")});
                let response = if open {
                    json!({"MediaSource":source})
                } else {
                    json!({"PlaySessionId":"failed-live","MediaSources":[{"Id":format!("{:032x}",1100),"LiveStreamId":"other-live"},source]})
                };
                Mock::given(method("POST"))
                    .and(path(if open {
                        "/LiveStreams/Open".into()
                    } else {
                        format!("/Items/{original}/PlaybackInfo")
                    }))
                    .respond_with(ResponseTemplate::new(200).set_body_json(response))
                    .expect(1)
                    .mount(&fixture.upstreams[0])
                    .await;
                let failure = if let Some(input) = input {
                    crate::application::playback::live::post_livestream_open_typed(
                        &fixture.state.playback_service(),
                        input,
                    )
                    .await
                    .unwrap_err()
                } else {
                    post_playback_info_typed(
                        &fixture.state.playback_service(),
                        operation(&fixture, "", json!({"MediaSourceId":fixture.sources[0]})),
                    )
                    .await
                    .unwrap_err()
                };
                assert_eq!(
                    failure.detail_status(),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "open={open}, translation={translation}"
                );
                // The service boundary must retain the real database cause,
                // including failures that travel through JSON field processing.
                let mut cause: &(dyn std::error::Error + 'static) = &failure;
                while !cause.is::<sqlx::Error>() {
                    cause = cause
                        .source()
                        .expect("database cause must survive workflow processing");
                }
                assert!(!format!("{failure:?}").contains("translation failure"));
                assert!(!format!("{failure:?}").contains("reconciliation failure"));
                // A failed old reservation must not clean up a replacement that
                // now owns the same backend live identity.
                if let Some(abandoned) = abandoned {
                    assert!(
                        !fixture
                            .state
                            .play_sessions
                            .commit_live_streams(&[abandoned])
                            .await
                    );
                }
                if let Some(id) = existing_id {
                    assert_eq!(id, plan.virtual_id());
                    let binding = fixture
                        .state
                        .play_sessions
                        .live_stream(&id, "viewer")
                        .await
                        .unwrap();
                    assert_eq!(binding.original_id.as_str(), "backend-live");
                } else {
                    assert!(fixture
                        .state
                        .play_sessions
                        .live_stream(plan.virtual_id(), "viewer")
                        .await
                        .is_none());
                }
                assert!(fixture
                    .state
                    .play_sessions
                    .get_session_for_user("failed-live", "viewer")
                    .await
                    .is_none());
                assert!(fixture
                    .state
                    .play_sessions
                    .live_stream(other_plan.virtual_id(), "viewer")
                    .await
                    .is_none());
                let after = fixture
                    .state
                    .play_sessions
                    .resolve_report_session("live-player", "viewer")
                    .await
                    .unwrap();
                assert_eq!(
                    before.as_ref().map(|binding| binding.revision()),
                    after.as_ref().map(|binding| binding.revision())
                );
            }
        }
    }
}

#[tokio::test]
async fn stale_reconciliation_drops_live_identity_with_its_unroutable_source() {
    for open in [false, true] {
        let fixture = fixture().await;
        let original = format!("{:032x}", 100);
        let input = if open {
            Some(live_open_input(&fixture, &original).await)
        } else {
            None
        };
        let plan = fixture
            .state
            .play_sessions
            .prepare_live_stream("stale-live", "viewer", fixture.sessions[0].1.id)
            .await;
        let source = "feedfeedfeedfeedfeedfeedfeedfeed";
        // Translation creates this mapping after the request's generation was
        // captured. Simulate a newer source inventory committing at that point.
        sqlx::query(&format!("CREATE TRIGGER newer_inventory AFTER INSERT ON media_mappings WHEN NEW.original_media_id = '{source}' BEGIN UPDATE movie_version_members SET sources_generation = 9000000000; END"))
            .execute(&fixture.pool).await.unwrap();
        let source = json!({"Id":source,"LiveStreamId":"stale-live"});
        Mock::given(method("POST"))
            .and(path(if open {
                "/LiveStreams/Open".into()
            } else {
                format!("/Items/{original}/PlaybackInfo")
            }))
            .respond_with(ResponseTemplate::new(200).set_body_json(if open {
                json!({"MediaSource":source})
            } else {
                json!({"PlaySessionId":"stale-player","MediaSources":[source]})
            }))
            .expect(1)
            .mount(&fixture.upstreams[0])
            .await;
        if let Some(input) = input {
            let before = fixture
                .state
                .play_sessions
                .resolve_report_session("live-player", "viewer")
                .await
                .unwrap()
                .unwrap()
                .revision();
            assert_eq!(
                crate::application::playback::live::post_livestream_open_typed(
                    &fixture.state.playback_service(),
                    input
                )
                .await
                .unwrap_err()
                .detail_status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
            assert_eq!(
                fixture
                    .state
                    .play_sessions
                    .resolve_report_session("live-player", "viewer")
                    .await
                    .unwrap()
                    .unwrap()
                    .revision(),
                before
            );
        } else {
            let response = post_playback_info_typed(
                &fixture.state.playback_service(),
                operation(&fixture, "", json!({"MediaSourceId":fixture.sources[0]})),
            )
            .await
            .unwrap();
            assert!(response.media_sources.is_empty());
        }
        assert!(fixture
            .state
            .play_sessions
            .live_stream(plan.virtual_id(), "viewer")
            .await
            .is_none());
    }
}

#[tokio::test]
async fn live_open_query_translates_owner_once_even_when_original_is_another_virtual_id() {
    let fixture = fixture().await;
    let server = &fixture.sessions[0].1;
    let other = fixture
        .state
        .media_storage
        .get_or_create_media_mapping("other-backend-item", server)
        .await
        .unwrap();
    let owner_original = other.virtual_media_id.clone();
    fixture
        .state
        .media_storage
        .get_or_create_media_mapping(&owner_original, server)
        .await
        .unwrap();
    let input = live_open_input(&fixture, &owner_original).await;
    Mock::given(method("POST"))
        .and(path("/LiveStreams/Open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"MediaSource":{"Id":"opened-source","LiveStreamId":"opened-live"}}),
        ))
        .expect(1)
        .mount(&fixture.upstreams[0])
        .await;
    let response = crate::application::playback::live::post_livestream_open_typed(
        &fixture.state.playback_service(),
        input,
    )
    .await
    .unwrap();
    let requests = fixture.upstreams[0].received_requests().await.unwrap();
    let request = requests
        .iter()
        .find(|request| request.method == "POST")
        .unwrap();
    assert_eq!(
        request
            .url
            .query_pairs()
            .find(|(key, _)| key == "ItemId")
            .unwrap()
            .1,
        owner_original
    );
    let payload: serde_json::Value = request.body_json().unwrap();
    assert_eq!(
        payload,
        json!({"ItemId":owner_original,"PlaySessionId":"live-player","UserId":"backend-viewer-0","OpenToken":"opaque-token"})
    );
    let binding = fixture
        .state
        .play_sessions
        .live_stream(
            response["MediaSource"]["LiveStreamId"].as_str().unwrap(),
            "viewer",
        )
        .await
        .unwrap();
    assert_eq!(binding.server_id, server.id);
    assert_eq!(
        fixture
            .state
            .play_sessions
            .get_session_for_user("live-player", "viewer")
            .await
            .unwrap()
            .original_item_id
            .as_deref(),
        Some(owner_original.as_str())
    );
    assert!(!fixture.upstreams[1]
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|request| request.method == "POST"));
}

#[tokio::test]
async fn live_actions_translate_colliding_live_identity_exactly_once() {
    use crate::application::playback::live::{post_live_stream_action_typed, LiveRequest};
    for action in ["Refresh", "Close"] {
        let fixture = fixture().await;
        let server = &fixture.sessions[0].1;
        let other = fixture
            .state
            .play_sessions
            .register_live_stream("other-original", "viewer", server.id)
            .await;
        let owner = fixture
            .state
            .play_sessions
            .register_live_stream(&other, "viewer", server.id)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/LiveStreams/{action}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"LiveStreamId":other})))
            .expect(1)
            .mount(&fixture.upstreams[0])
            .await;
        let request = reqwest::Client::new()
            .post(format!(
                "http://proxy/LiveStreams/{action}?LiveStreamId={owner}"
            ))
            .json(&json!({"LiveStreamId":owner}))
            .build()
            .unwrap();
        post_live_stream_action_typed(
            &fixture.state.playback_service(),
            LiveRequest {
                original_request: request,
                context: operation(&fixture, "", json!({})).context,
            },
        )
        .await
        .unwrap();
        let requests = fixture.upstreams[0].received_requests().await.unwrap();
        let request = requests
            .iter()
            .find(|request| request.method == "POST")
            .unwrap();
        assert_eq!(
            request
                .url
                .query_pairs()
                .find(|(key, _)| key == "LiveStreamId")
                .unwrap()
                .1,
            other
        );
        assert_eq!(
            request.body_json::<serde_json::Value>().unwrap(),
            json!({"LiveStreamId":other})
        );
        assert!(fixture
            .state
            .play_sessions
            .live_stream(&other, "viewer")
            .await
            .is_some());
    }
}

#[tokio::test]
async fn successful_live_workflows_publish_the_exact_advertised_identity_and_urls() {
    for open in [false, true] {
        let fixture = fixture().await;
        let server = &fixture.sessions[0].1;
        let original = format!("{:032x}", 100);
        let input = if open {
            Some(live_open_input(&fixture, &original).await)
        } else {
            None
        };
        let id = fixture
            .state
            .play_sessions
            .register_live_stream("stable-live", "viewer", server.id)
            .await;
        let source = json!({
            "Id":format!("{:032x}", 1100), "LiveStreamId":"stable-live",
            "StreamUrl":format!("/Videos/{original}/stream?LiveStreamId=stable-live&Static=true"),
            "TranscodingUrl":format!("/Videos/{original}/master.m3u8?livestreamid=stable-live&PlaySessionId=successful-live"),
            "MediaStreams":[{"Index":1,"DeliveryUrl":format!("/Videos/{original}/Subtitles/1/Stream?LiveStreamId=stable-live")}],
            "MediaAttachments":[{"DeliveryUrl":format!("/Videos/{original}/Attachments/1?LiveStreamId=stable-live")}]
        });
        Mock::given(method("POST"))
            .and(path(if open {
                "/LiveStreams/Open".into()
            } else {
                format!("/Items/{original}/PlaybackInfo")
            }))
            .respond_with(ResponseTemplate::new(200).set_body_json(if open {
                json!({"MediaSource":source})
            } else {
                json!({"PlaySessionId":"successful-live","MediaSources":[source]})
            }))
            .expect(1)
            .mount(&fixture.upstreams[0])
            .await;
        let source = if let Some(input) = input {
            crate::application::playback::live::post_livestream_open_typed(
                &fixture.state.playback_service(),
                input,
            )
            .await
            .unwrap()["MediaSource"]
                .clone()
        } else {
            serde_json::to_value(
                post_playback_info_typed(
                    &fixture.state.playback_service(),
                    operation(&fixture, "", json!({"MediaSourceId":fixture.sources[0]})),
                )
                .await
                .unwrap(),
            )
            .unwrap()["MediaSources"][0]
                .clone()
        };
        assert_eq!(source["LiveStreamId"], id);
        for url in [
            source["StreamUrl"].as_str().unwrap(),
            source["TranscodingUrl"].as_str().unwrap(),
            source["MediaStreams"][0]["DeliveryUrl"].as_str().unwrap(),
            source["MediaAttachments"][0]["DeliveryUrl"]
                .as_str()
                .unwrap(),
        ] {
            let url = url::Url::parse(&format!("http://proxy{url}")).unwrap();
            assert_eq!(
                url.query_pairs()
                    .find(|(key, _)| key.eq_ignore_ascii_case("LiveStreamId"))
                    .unwrap()
                    .1,
                id
            );
        }
        assert_eq!(
            fixture
                .state
                .play_sessions
                .live_stream(&id, "viewer")
                .await
                .unwrap()
                .original_id,
            "stable-live".into()
        );
    }
}
