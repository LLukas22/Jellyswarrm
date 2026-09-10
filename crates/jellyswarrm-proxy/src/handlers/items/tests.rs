use std::{collections::BTreeSet, sync::Arc};

use super::*;
use crate::{
    config::{AppConfig, MediaStreamingMode, MIGRATOR},
    handlers::quick_connect::QuickConnectStorage,
    media_identity::{MediaAlias, MediaKind, MediaObservation, MediaProvider},
    media_storage_service::{MediaCatalogSnapshot, MediaStorageService},
    request_preprocessing::apply_to_request,
    server_storage::ServerStorageService,
    session_storage::SessionStorage,
    user_authorization_service::{AuthorizationSession, Device, UserAuthorizationService},
    virtual_library_service::{VirtualLibraryAccessScope, VirtualLibraryService},
    DataContext, ProxyProcessors,
};
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

#[tokio::test]
async fn selected_source_detail_fetches_owner_and_keeps_versions_across_equivalent_scopes() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let server_storage = ServerStorageService::new(pool.clone());
    let media_storage = MediaStorageService::new(pool.clone());
    let context = DataContext {
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
    let state = AppState::new(
        reqwest::Client::new(),
        reqwest::Client::new(),
        context.clone(),
        ProxyProcessors::new(context),
        QuickConnectStorage::new(),
    );
    let mut upstreams = Vec::new();
    let mut sessions = Vec::new();
    let mut owners = Vec::new();
    let mut sources = Vec::new();
    let mut snapshots = Vec::new();
    for index in 0..2 {
        let owner_id = format!("{:032x}", 100 + index);
        let source_id = format!("{:032x}", 200 + index);
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/System/Info/Public"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ServerName": "Upstream"})),
            )
            .mount(&upstream)
            .await;
        // Only the owner endpoint exists. Fetching the source ID fails.
        Mock::given(method("GET"))
            .and(path(format!("/Items/{owner_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Id": owner_id, "Name": format!("Owner {index}"),
                "Type": "Movie", "Overview": format!("Selected metadata {index}"),
                "MediaSources": [{"Id": source_id, "Name": "Version", "Type": "Default",
                    "Path": format!("/media/{index}.mkv"), "SupportsDirectPlay": true,
                    "DefaultAudioStreamIndex": index,
                    "MediaStreams": [{"Type": "Audio", "Index": index, "Language": "deu"}]}]
            })))
            .expect(4)
            .mount(&upstream)
            .await;
        let server_id = state
            .server_storage
            .add_server(
                &format!("Server {index}"),
                &upstream.uri(),
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = state
            .server_storage
            .get_server_by_id(server_id)
            .await
            .unwrap()
            .unwrap();
        let owner = state
            .media_storage
            .get_or_create_media_mapping(&owner_id, &server)
            .await
            .unwrap();
        let source = state
            .media_storage
            .get_or_create_media_mapping(&source_id, &server)
            .await
            .unwrap();
        snapshots.push(MediaCatalogSnapshot {
            source_key: upstream.uri(),
            server_id: server.id,
            complete: true,
            observations: vec![MediaObservation {
                virtual_media_id: owner.virtual_media_id.clone(),
                aliases: BTreeSet::from([MediaAlias {
                    provider: MediaProvider::Tmdb,
                    kind: MediaKind::Movie,
                    provider_id: "42".into(),
                }]),
            }],
        });
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
                    device_id: "test".into(),
                    version: "1".into(),
                },
                jellyfin_token: "upstream-token".into(),
                original_user_id: "backend-user".into(),
                expires_at: None,
                created_at: now,
                updated_at: now,
            },
            server,
        ));
        owners.push(owner);
        sources.push(source);
        upstreams.push(upstream);
    }
    state.server_storage.check_servers_health().await;
    let mut groups = Vec::new();
    for scope in ["automatic:library:viewer", "latest:viewer:"] {
        let generation = state
            .media_storage
            .begin_media_reconciliation()
            .await
            .unwrap();
        let assignments = state
            .media_storage
            .reconcile_media_catalog(scope, generation, &snapshots, true)
            .await
            .unwrap();
        groups.push(
            assignments[&owners[0].virtual_media_id]
                .virtual_media_id
                .clone(),
        );
    }
    let scope =
        VirtualLibraryAccessScope::new("viewer", sessions.iter().map(|(_, server)| server.id));
    // Expose both scopes, select the second source, then switch back to the first.
    for (case, requested_id, selected, expected_id) in [
        ("browse aggregate", &groups[0], 0, &groups[0]),
        ("latest aggregate", &groups[1], 0, &groups[1]),
        (
            "select second version",
            &sources[1].virtual_media_id,
            1,
            &owners[1].virtual_media_id,
        ),
        (
            "switch back to first",
            &sources[0].virtual_media_id,
            0,
            &owners[0].virtual_media_id,
        ),
    ] {
        let (session, server) = &sessions[selected];
        let original_request = reqwest::Request::new(
            reqwest::Method::GET,
            format!("http://localhost/Items/{requested_id}?userId=viewer")
                .parse()
                .unwrap(),
        );
        let mut request = original_request.try_clone().unwrap();
        apply_to_request(
            &mut request,
            server,
            &Some(session.clone()),
            &None,
            &state,
            Some(&scope),
        )
        .await;
        let Json(response) = get_item(
            State(state.clone()),
            Preprocessed(PreprocessedRequest {
                request,
                original_request,
                user: None,
                sessions: Some(sessions.clone()),
                server: server.clone(),
                auth: None,
                session: Some(session.clone()),
                new_auth: None,
                access_scope: Some(scope.clone()),
                server_matched_request: true,
                pending_playback_session_update: None,
            }),
        )
        .await
        .unwrap_or_else(|status| panic!("{case}: detail returned {status}"));
        assert_eq!(response["Id"], *expected_id, "{case}");
        assert_eq!(response["Name"], format!("Owner {selected}"), "{case}");
        assert_eq!(
            response["Overview"],
            format!("Selected metadata {selected}"),
            "{case}"
        );
        assert_eq!(response["MediaSourceCount"], 2, "{case}");
        let merged = response["MediaSources"].as_array().unwrap();
        assert_eq!(merged.len(), 2, "{case}");
        assert_eq!(
            merged[0]["Id"], sources[selected].virtual_media_id,
            "{case}"
        );
        assert_eq!(merged[0]["Type"], "Default");
        assert_eq!(merged[0]["Path"], format!("/media/{selected}.mkv"));
        assert_eq!(merged[0]["DefaultAudioStreamIndex"], selected);
        assert_eq!(merged[0]["MediaStreams"][0]["Language"], "deu");
        assert_eq!(merged[0]["SupportsDirectPlay"], true);
        assert_eq!(
            merged[1]["Id"],
            sources[1 - selected].virtual_media_id,
            "{case}"
        );
        assert_eq!(merged[1]["Type"], "Grouping");
    }
    for upstream in upstreams {
        upstream.verify().await;
    }
}
