use std::sync::Arc;

use super::*;
use crate::{
    config::{AppConfig, MIGRATOR},
    handlers::quick_connect::QuickConnectStorage,
    media_storage_service::MediaStorageService,
    server_storage::{Server, ServerStorageService},
    session_storage::SessionStorage,
    user_authorization_service::{AuthorizationSession, Device, UserAuthorizationService},
    virtual_library_service::{VirtualLibraryAccessScope, VirtualLibraryService},
    DataContext, ProxyProcessors,
};
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

pub(super) async fn setup() -> (
    AppState,
    sqlx::SqlitePool,
    Vec<(AuthorizationSession, Server)>,
    Vec<MockServer>,
) {
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
            pool.clone(),
            server_storage,
            media_storage,
        )),
        play_sessions: Arc::new(SessionStorage::new()),
        config: Arc::new(tokio::sync::RwLock::new(AppConfig {
            deduplicate_media: true,
            merge_libraries: false,
            ..AppConfig::default()
        })),
    };
    let processors = ProxyProcessors::new(context.clone());
    let state = AppState::new(
        reqwest::Client::new(),
        reqwest::Client::new(),
        context,
        processors,
        QuickConnectStorage::new(),
    );
    let mut sessions = Vec::new();
    let mut upstreams = Vec::new();
    for index in 0..2 {
        let upstream = MockServer::start().await;
        let row = sqlx::query("INSERT INTO servers (name, url, priority, created_at, updated_at) VALUES (?, ?, 100, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) RETURNING *")
            .bind(format!("Server {index}"))
            .bind(upstream.uri())
            .fetch_one(&pool).await.unwrap();
        let server = Server::from_row(row).unwrap();
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
        upstreams.push(upstream);
    }
    (state, pool, sessions, upstreams)
}

pub(super) fn request(
    path: &str,
    sessions: &[(AuthorizationSession, Server)],
) -> PreprocessedRequest {
    let original_request = reqwest::Request::new(
        reqwest::Method::GET,
        url::Url::parse(&format!("http://localhost{path}")).unwrap(),
    );
    PreprocessedRequest {
        request: original_request.try_clone().unwrap(),
        original_request,
        user: None,
        sessions: Some(sessions.to_vec()),
        server: sessions[0].1.clone(),
        auth: None,
        session: Some(sessions[0].0.clone()),
        new_auth: None,
        access_scope: Some(VirtualLibraryAccessScope::new(
            "viewer",
            sessions.iter().map(|(_, server)| server.id),
        )),
        server_matched_request: false,
        pending_playback_session_update: None,
    }
}

fn latest_items() -> Value {
    json!([
        {"Id":"movie", "Name":"Movie", "Type":"Movie", "ProviderIds":{"Tmdb":"42"}, "DateCreated":"2026-01-01T00:00:00Z"},
        {"Id":"series", "Name":"Show", "Type":"Series", "ProviderIds":{"Tmdb":"42"}, "DateCreated":"2026-02-01T00:00:00Z"},
        {"Id":"unknown", "Name":"Unidentified", "Type":"Movie", "DateCreated":"2025-01-01T00:00:00Z"}
    ])
}

fn library_items(count: usize) -> Vec<Value> {
    (0..count)
        .map(|index| {
            json!({
                "Id": format!("library-{index}"), "Name": format!("Library {index}"),
                "Type": "CollectionFolder", "CollectionType": "movies"
            })
        })
        .collect()
}

#[tokio::test]
async fn library_inventory_fetches_capped_pages_before_client_pagination() {
    for merge_libraries in [false, true] {
        let (state, _pool, sessions, upstreams) = setup().await;
        state.config.write().await.merge_libraries = merge_libraries;
        let libraries = library_items(45);
        Mock::given(method("GET"))
            .respond_with(move |request: &wiremock::Request| {
                let start = request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key.eq_ignore_ascii_case("StartIndex"))
                    .and_then(|(_, value)| value.parse::<usize>().ok())
                    .unwrap_or(0);
                ResponseTemplate::new(200).set_body_json(json!({
                    "Items": libraries.iter().skip(start).take(20).collect::<Vec<_>>(),
                    "TotalRecordCount": 45, "StartIndex": start
                }))
            })
            .expect(3)
            .mount(&upstreams[0])
            .await;
        let Json(response) = get_items_from_all_servers_preprocessed(
            &state,
            request("/Users/viewer/Views?StartIndex=20&Limit=1", &sessions[..1]),
        )
        .await
        .unwrap();
        assert_eq!(response["Items"].as_array().unwrap().len(), 1);
        assert_eq!(response["TotalRecordCount"], 45);
        let discovered = state
            .virtual_library_service
            .list_discovered_libraries()
            .await
            .unwrap();
        assert_eq!(discovered.len(), 45);
        assert!(discovered
            .iter()
            .any(|library| library.original_library_id == "library-44"));
    }
}

#[tokio::test]
async fn incomplete_library_inventory_is_not_cached() {
    for failure in ["repeated", "empty", "http", "changed"] {
        let (state, _pool, sessions, upstreams) = setup().await;
        state.config.write().await.merge_libraries = true;
        let libraries = library_items(20);
        Mock::given(method("GET"))
            .respond_with(move |request: &wiremock::Request| {
                let continuation = request
                    .url
                    .query_pairs()
                    .any(|(key, _)| key == "StartIndex");
                if continuation && failure == "http" {
                    return ResponseTemplate::new(500);
                }
                let items = if continuation && failure == "empty" {
                    vec![]
                } else {
                    libraries.clone()
                };
                ResponseTemplate::new(200).set_body_json(json!({
                    "Items": items, "TotalRecordCount": if continuation && failure == "changed" { 30 } else { 25 }, "StartIndex": 0
                }))
            })
            .expect(2)
            .mount(&upstreams[0])
            .await;
        let result = get_items_from_all_servers_preprocessed(
            &state,
            request("/Users/viewer/Views", &sessions[..1]),
        )
        .await;
        assert!(result.is_err(), "{failure}");
        assert!(state
            .virtual_library_service
            .list_discovered_libraries()
            .await
            .unwrap()
            .is_empty());
    }
}

#[tokio::test]
async fn admin_library_page_discovers_mapped_users_without_prior_catalog_requests() {
    use axum::response::IntoResponse;
    let (mut state, pool, sessions, upstreams) = setup().await;
    state.user_authorization = Arc::new(UserAuthorizationService::with_mapping_key(
        pool,
        crate::encryption::MappingEncryptionKey::from_session_key(&[7; 64]).unwrap(),
        (&state.get_admin_password().await).into(),
    ));
    let server = &sessions[0].1;
    Mock::given(method("GET"))
        .and(path("/System/Info/Public"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ServerName":"Test"})))
        .mount(&upstreams[0])
        .await;
    for (index, username) in ["restricted", "complete"].into_iter().enumerate() {
        let user = state
            .user_authorization
            .create_user(username, &"password".into())
            .await
            .unwrap();
        state
            .user_authorization
            .add_server_mapping(&user.id, server, username, &"password".into(), None)
            .await
            .unwrap();
        let backend_id = format!("user-{index}");
        Mock::given(method("POST")).and(path("/Users/AuthenticateByName"))
            .and(wiremock::matchers::body_json(json!({"Username": username, "Pw": "password"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "User": {"Id": backend_id, "Name": username}, "AccessToken": "token", "ServerId":"test"
            }))).mount(&upstreams[0]).await;
        let libraries = library_items(if index == 0 { 1 } else { 25 });
        Mock::given(method("GET"))
            .and(path(format!("/Users/{backend_id}/Views")))
            .respond_with(move |request: &wiremock::Request| {
                let start = request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == "StartIndex")
                    .and_then(|(_, value)| value.parse::<usize>().ok())
                    .unwrap_or(0);
                ResponseTemplate::new(200).set_body_json(json!({
                    "Items": libraries.iter().skip(start).take(20).collect::<Vec<_>>(),
                    "TotalRecordCount": libraries.len()
                }))
            })
            .mount(&upstreams[0])
            .await;
    }
    assert!(state
        .virtual_library_service
        .list_discovered_libraries()
        .await
        .unwrap()
        .is_empty());
    let response = crate::ui::admin::libraries::library_groups_list(State(state.clone()))
        .await
        .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(html.matches("data-library-card").count(), 25, "{html}");
    assert!(html.contains("Library 24"));
    assert!(!html.contains("role=\"alert\""), "{html}");
    assert!(html.contains("id=\"library-discovery-errors\""));
    assert_eq!(
        state
            .virtual_library_service
            .list_discovered_libraries()
            .await
            .unwrap()
            .len(),
        25
    );

    // Editing groups uses the completed cache, not another upstream discovery.
    use crate::ui::admin::libraries::{
        assign_library, create_group, delete_group, remove_member, rename_group, AssignLibraryForm,
        CreateGroupForm, RemoveMemberForm, RenameGroupForm,
    };
    let request_count = upstreams[0].received_requests().await.unwrap().len();
    let mut responses = vec![
        create_group(
            State(state.clone()),
            axum::Form(CreateGroupForm {
                name: "Test group".into(),
            }),
        )
        .await,
    ];
    let group_id = state.virtual_library_service.list_groups().await.unwrap()[0]
        .virtual_id
        .clone();
    responses.push(
        assign_library(
            State(state.clone()),
            axum::Form(AssignLibraryForm {
                group_virtual_id: group_id.clone(),
                server_id: server.id.as_i64(),
                library_id: "library-24".into(),
            }),
        )
        .await,
    );
    responses.push(
        rename_group(
            State(state.clone()),
            axum::extract::Path(group_id.clone()),
            axum::Form(RenameGroupForm {
                name: "Renamed group".into(),
            }),
        )
        .await,
    );
    responses.push(
        remove_member(
            State(state.clone()),
            axum::extract::Path(group_id.clone()),
            axum::Form(RemoveMemberForm {
                server_id: server.id.as_i64(),
                library_id: "library-24".into(),
            }),
        )
        .await,
    );
    responses.push(delete_group(State(state.clone()), axum::extract::Path(group_id)).await);
    for response in responses {
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(html.matches("data-library-card").count(), 25);
        assert!(html.contains("Library 24"));
        assert!(!html.contains("id=\"library-discovery-errors\""));
    }
    assert_eq!(
        upstreams[0].received_requests().await.unwrap().len(),
        request_count
    );

    // An explicit refresh still contacts upstreams and reports failures.
    upstreams[0].reset().await;
    let response = crate::ui::admin::libraries::library_groups_list(State(state.clone()))
        .await
        .into_response();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(html.matches("data-library-card").count(), 25);
    assert!(html.contains("role=\"alert\""));
    assert_eq!(
        state
            .virtual_library_service
            .list_discovered_libraries()
            .await
            .unwrap()
            .len(),
        25
    );
}

// Exercise the production HTTP handler and preprocessing, not just cmp_by.
// Jellyfin only includes SortName and DateCreated when requested in Fields.
async fn check_web_album_sorting(sort_by: &str, ascending_names: &[&str]) {
    let (state, _pool, sessions, upstreams) = setup().await;
    let user = state
        .user_authorization
        .create_user("album-viewer", &"password".into())
        .await
        .unwrap();
    let authorization = crate::models::Authorization {
        client: "album-test".into(),
        device: "test".into(),
        device_id: "album-test".into(),
        version: "1".into(),
        token: None,
    };
    let albums = [
        json!([
            {"Id":"a", "Type":"MusicAlbum", "Name":"The Zebra", "SortName":"aardvark", "AlbumArtist":"Shared artist", "DateCreated":"2026-01-01T00:00:00Z"},
            {"Id":"c", "Type":"MusicAlbum", "Name":"Omega", "SortName":"omega", "AlbumArtist":"Z artist", "DateCreated":"2026-03-01T00:00:00Z"}
        ]),
        json!([
            {"Id":"b", "Type":"MusicAlbum", "Name":"Alpha", "SortName":"zulu", "AlbumArtist":"Shared artist", "DateCreated":"2026-01-01T00:00:00Z"},
            {"Id":"d", "Type":"MusicAlbum", "Name":"Beta", "SortName":"beta", "AlbumArtist":"A artist", "DateCreated":"2026-02-01T00:00:00Z"}
        ]),
    ];
    for (index, (_, server)) in sessions.iter().enumerate() {
        state
            .user_authorization
            .add_server_mapping(&user.id, server, "album-viewer", &"password".into(), None)
            .await
            .unwrap();
        state
            .user_authorization
            .store_authorization_session(
                &user.id,
                server,
                &authorization,
                "upstream-token".into(),
                "backend-user".into(),
                None,
            )
            .await
            .unwrap();
        Mock::given(method("GET"))
            .and(path("/System/Info/Public"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ServerName":"Test"})))
            .mount(&upstreams[index])
            .await;
        let albums = albums[index].as_array().unwrap().clone();
        Mock::given(method("GET"))
            .and(path("/Items"))
            .respond_with(move |request: &wiremock::Request| {
                let fields = request
                    .url
                    .query_pairs()
                    .filter(|(key, _)| key.eq_ignore_ascii_case("Fields"))
                    .flat_map(|(_, value)| value.split(',').map(str::to_owned).collect::<Vec<_>>())
                    .collect::<Vec<_>>();
                let mut items = albums.clone();
                for item in &mut items {
                    for field in ["SortName", "DateCreated"] {
                        if !fields
                            .iter()
                            .any(|requested| requested.eq_ignore_ascii_case(field))
                        {
                            item.as_object_mut().unwrap().remove(field);
                        }
                    }
                }
                ResponseTemplate::new(200).set_body_json(json!({
                    "Items":items, "TotalRecordCount":items.len(), "StartIndex":0
                }))
            })
            .mount(&upstreams[index])
            .await;
    }
    state.server_storage.check_servers_health().await;
    let app = axum::Router::new()
        .route(
            "/Items",
            axum::routing::get(get_items_from_all_servers_if_not_restricted),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // Modern web requests use camelCase and don't request SortName. Also cover
    // legacy PascalCase requests and pagination after the global sort.
    for (sort_key, order_key, fields_key) in [
        ("sortBy", "sortOrder", "fields"),
        ("SortBy", "SortOrder", "Fields"),
    ] {
        for order in ["Ascending", "Descending"] {
            let mut expected = ascending_names.to_vec();
            if order == "Descending" {
                expected.reverse();
            }
            for (start, limit) in [(0, 4), (1, 2)] {
                let mut request = state.reqwest_client.get(format!("{base_url}/Items"))
                    .header("Authorization", format!("MediaBrowser Client=\"album-test\", Device=\"test\", DeviceId=\"album-test\", Version=\"1\", Token=\"{}\"", user.virtual_key))
                    .query(&[
                        ("userId", user.id.as_str()), ("includeItemTypes", "MusicAlbum"),
                        ("recursive", "true"), (order_key, order),
                        (fields_key, if sort_key == "sortBy" { "PrimaryImageAspectRatio" } else { "PrimaryImageAspectRatio,SortName" }),
                        ("startIndex", &start.to_string()), ("limit", &limit.to_string()),
                    ]);
                if sort_key == "sortBy" {
                    // The Jellyfin SDK encodes array-valued sorts as repeated keys.
                    for field in sort_by.split(',') {
                        request = request.query(&[(sort_key, field)]);
                    }
                } else {
                    request = request.query(&[(sort_key, sort_by)]);
                }
                let response = request.send().await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let response: Value = response.json().await.unwrap();
                let names = response["Items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| {
                        item["Name"]
                            .as_str()
                            .unwrap()
                            .split(" [Server ")
                            .next()
                            .unwrap()
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    names,
                    expected[start..start + limit],
                    "{sort_by} {order}, start={start}: {response}"
                );
                assert_eq!(response["TotalRecordCount"], 4);
                assert_eq!(response["StartIndex"], start);
            }
        }
    }
    task.abort();
}

#[tokio::test]
async fn web_album_name_sorting_http_integration() {
    check_web_album_sorting("SortName", &["The Zebra", "Beta", "Omega", "Alpha"]).await;
}

#[tokio::test]
async fn web_album_date_added_sorting_http_integration() {
    check_web_album_sorting(
        "DateCreated,SortName",
        &["The Zebra", "Alpha", "Beta", "Omega"],
    )
    .await;
}

#[tokio::test]
async fn web_album_artist_sorting_http_integration() {
    check_web_album_sorting(
        "AlbumArtist,SortName",
        &["Beta", "The Zebra", "Alpha", "Omega"],
    )
    .await;
}

#[tokio::test]
async fn series_tv_schedule_only_queries_the_series_server() {
    let (state, _pool, sessions, upstreams) = setup().await;
    let mapping = state
        .media_storage
        .get_or_create_media_mapping("upstream-series", &sessions[1].1)
        .await
        .unwrap();
    let url = url::Url::parse(&format!(
        "http://localhost/LiveTv/Programs?LibrarySeriesId={}",
        mapping.virtual_media_id
    ))
    .unwrap();
    let scope =
        VirtualLibraryAccessScope::new("viewer", sessions.iter().map(|(_, server)| server.id));
    let server = state
        .processors
        .url_processor
        .server_from_client_url(&url, Some(&scope))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(server.id, sessions[1].1.id);

    Mock::given(method("GET"))
        .and(path("/LiveTv/Programs"))
        .and(query_param("LibrarySeriesId", "upstream-series"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Items": [{"Id": "matching-program", "Type": "Program"}],
            "TotalRecordCount": 1,
            "StartIndex": 0
        })))
        .expect(1)
        .mount(&upstreams[1])
        .await;

    let mut preprocessed = request(
        &format!("{}?{}", url.path(), url.query().unwrap()),
        &sessions,
    );
    preprocessed.server = server.clone();
    preprocessed.session = Some(sessions[1].0.clone());
    crate::request_preprocessing::apply_to_request(
        &mut preprocessed.request,
        &server,
        &preprocessed.session,
        &None,
        &state,
        preprocessed.access_scope.as_ref(),
    )
    .await;
    let Json(response) = get_live_tv_programs(State(state), Preprocessed(preprocessed))
        .await
        .unwrap();
    assert_eq!(response["Items"].as_array().unwrap().len(), 1);
    assert!(upstreams[0].received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn channel_tv_schedule_only_queries_the_channel_server() {
    let (state, _pool, sessions, upstreams) = setup().await;
    let mapping = state
        .media_storage
        .get_or_create_media_mapping("upstream-channel", &sessions[1].1)
        .await
        .unwrap();
    let url = url::Url::parse(&format!(
        "http://localhost/LiveTv/Programs?ChannelIds={}",
        mapping.virtual_media_id
    ))
    .unwrap();
    let scope =
        VirtualLibraryAccessScope::new("viewer", sessions.iter().map(|(_, server)| server.id));
    let server = state
        .processors
        .url_processor
        .server_from_client_url(&url, Some(&scope))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(server.id, sessions[1].1.id);

    Mock::given(method("GET"))
        .and(path("/LiveTv/Programs"))
        .and(query_param("ChannelIds", "upstream-channel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Items": [{"Id": "channel-program", "Type": "Program"}],
            "TotalRecordCount": 1,
            "StartIndex": 0
        })))
        .expect(1)
        .mount(&upstreams[1])
        .await;

    let mut preprocessed = request(
        &format!("{}?{}", url.path(), url.query().unwrap()),
        &sessions,
    );
    preprocessed.server = server.clone();
    preprocessed.session = Some(sessions[1].0.clone());
    crate::request_preprocessing::apply_to_request(
        &mut preprocessed.request,
        &server,
        &preprocessed.session,
        &None,
        &state,
        preprocessed.access_scope.as_ref(),
    )
    .await;
    let Json(response) = get_live_tv_programs(State(state), Preprocessed(preprocessed))
        .await
        .unwrap();
    assert_eq!(response["Items"].as_array().unwrap().len(), 1);
    assert!(upstreams[0].received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn guide_programs_remap_channel_ids_on_each_server() {
    let (state, _pool, sessions, upstreams) = setup().await;
    let mut channel_ids = Vec::new();
    let mut timer_ids = Vec::new();
    for (index, (_, server)) in sessions.iter().enumerate() {
        let original_id = format!("channel-{index}");
        let mapping = state
            .media_storage
            .get_or_create_media_mapping(&original_id, server)
            .await
            .unwrap();
        channel_ids.push(mapping.virtual_media_id);
        timer_ids.push(
            state
                .media_storage
                .get_or_create_media_mapping(&format!("timer-{index}"), server)
                .await
                .unwrap()
                .virtual_media_id,
        );
        Mock::given(method("GET"))
            .and(path("/LiveTv/Programs"))
            .respond_with(move |request: &wiremock::Request| {
                let ids = request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key.eq_ignore_ascii_case("ChannelIds"))
                    .map(|(_, value)| value.into_owned())
                    .unwrap_or_default();
                if !ids.split(',').any(|id| id == original_id) {
                    return ResponseTemplate::new(400);
                }
                ResponseTemplate::new(200).set_body_json(json!({
                    "Items": [{"Id": format!("program-{index}"), "Type": "Program", "ChannelId": original_id,
                        "SeriesTimerId": format!("timer-{index}")}],
                    "TotalRecordCount": 1,
                    "StartIndex": 0
                }))
            })
            .expect(1)
            .mount(&upstreams[index])
            .await;
    }
    let path = format!("/LiveTv/Programs?ChannelIds={}", channel_ids.join(","));
    let Json(response) = get_live_tv_programs(
        State(state.clone()),
        Preprocessed(request(&path, &sessions)),
    )
    .await
    .unwrap();
    assert_eq!(response["Items"].as_array().unwrap().len(), 2);
    let returned_channels = response["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|program| program["ChannelId"].as_str().unwrap())
        .collect::<Vec<_>>();
    for channel_id in &channel_ids {
        assert!(returned_channels.contains(&channel_id.as_str()));
    }
    let returned_timers = response["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|program| program["SeriesTimerId"].as_str().unwrap())
        .collect::<Vec<_>>();
    for (index, timer_id) in timer_ids.iter().enumerate() {
        assert!(returned_timers.contains(&timer_id.as_str()));
        let mut timer_url = url::Url::parse(&format!(
            "http://localhost/LiveTv/Timers?SeriesTimerId={timer_id}"
        ))
        .unwrap();
        let server = state
            .processors
            .url_processor
            .server_from_client_url(&timer_url, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(server.id, sessions[index].1.id);
        state
            .processors
            .url_processor
            .client_to_server_url(&mut timer_url, &None, None, Some(server.id))
            .await;
        assert_eq!(
            timer_url
                .query_pairs()
                .find(|(key, _)| key == "SeriesTimerId")
                .unwrap()
                .1,
            format!("timer-{index}")
        );
    }
}

#[tokio::test]
async fn show_navigation_preserves_parents_and_skips_servers_without_the_season() {
    check_show_season_navigation(false).await;
}

#[tokio::test]
async fn merged_library_show_routes_merge_localized_seasons_and_keep_navigation() {
    let (state, _pool, sessions, upstreams) = setup().await;
    state.config.write().await.include_server_name_in_media = true;
    let library = state
        .virtual_library_service
        .create_group("Shows")
        .await
        .unwrap();
    for (index, (_, server)) in sessions.iter().enumerate() {
        state
            .virtual_library_service
            .add_member(
                &library.virtual_id,
                server.id,
                &format!("library-{index}"),
                "Shows",
                "tvshows",
            )
            .await
            .unwrap();
        Mock::given(method("GET"))
            .and(path("/Items"))
            .and(query_param("ParentId", format!("library-{index}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": format!("series-{index}"), "Type": "Series", "Name": "Death Note",
                    "ProviderIds": if index == 0 { json!({"Imdb": "tt0877057", "Tmdb": "13916"}) }
                                   else { json!({"Imdb": "tt0877057"}) }}],
                "TotalRecordCount": 1, "StartIndex": 0
            })))
            .mount(&upstreams[index])
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/Shows/series-{index}/Seasons")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": format!("season-{index}"), "Type": "Season",
                    "SeriesId": format!("series-{index}"), "ParentId": format!("series-{index}"),
                    "Name": if index == 0 { "Season 1" } else { "Staffel 1" }, "IndexNumber": 1}],
                "TotalRecordCount": 1, "StartIndex": 0
            })))
            .mount(&upstreams[index])
            .await;
        Mock::given(method("GET"))
            .and(path("/Items"))
            .and(query_param("ParentId", format!("series-{index}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": format!("season-{index}"), "Type": "Season",
                    "SeriesId": format!("series-{index}"), "ParentId": format!("series-{index}"),
                    "Name": if index == 0 { "Season 1" } else { "Staffel 1" }, "IndexNumber": 1}],
                "TotalRecordCount": 1, "StartIndex": 0
            })))
            .mount(&upstreams[index])
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/Shows/series-{index}/Episodes")))
            .and(query_param("SeasonId", format!("season-{index}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": format!("episode-{index}"), "Type": "Episode",
                    "SeriesId": format!("series-{index}"), "SeasonId": format!("season-{index}"),
                    "ParentId": format!("season-{index}"), "ParentIndexNumber": 1, "IndexNumber": 1,
                    "Name": if index == 0 { "Rebirth" } else { "Wiedergeburt" },
                    "ProviderIds": if index == 0 { json!({"Imdb": "tt0967942"}) } else { json!({}) }}],
                "TotalRecordCount": 1, "StartIndex": 0
            })))
            .mount(&upstreams[index])
            .await;
    }
    let Json(shows) = get_items_from_all_servers_preprocessed(
        &state,
        request(
            &format!(
                "/Items?ParentId={}&IncludeItemTypes=Series",
                library.virtual_id
            ),
            &sessions,
        ),
    )
    .await
    .unwrap();
    assert_eq!(shows["Items"].as_array().unwrap().len(), 1);
    let series_id = shows["Items"][0]["Id"].as_str().unwrap();
    let path = format!("/Shows/{series_id}/Seasons?UserId=viewer&Fields=ItemCounts,PrimaryImageAspectRatio,CanDelete,MediaSourceCount");
    let mut season_id = String::new();
    for _ in 0..2 {
        let Json(seasons) = get_show_children_from_all_servers(
            State(state.clone()),
            Preprocessed(request(&path, &sessions)),
        )
        .await
        .unwrap();
        assert_eq!(seasons["TotalRecordCount"], 1);
        assert_eq!(seasons["Items"].as_array().unwrap().len(), 1);
        let season = &seasons["Items"][0];
        assert_eq!(season["SeriesId"], series_id);
        assert_eq!(season["ParentId"], series_id);
        assert_eq!(season["IndexNumber"], 1);
        assert!(matches!(
            season["Name"].as_str(),
            Some("Season 1" | "Staffel 1")
        ));
        let id = season["Id"].as_str().unwrap();
        if !season_id.is_empty() {
            assert_eq!(id, season_id);
        }
        season_id = id.to_string();
        assert_eq!(
            state
                .media_storage
                .get_media_version_members_by_virtual_id(id)
                .await
                .unwrap()
                .len(),
            2
        );
    }
    let Json(items_seasons) = get_items_from_all_servers_preprocessed(
        &state,
        request(
            &format!("/Items?ParentId={series_id}&IncludeItemTypes=Season"),
            &sessions,
        ),
    )
    .await
    .unwrap();
    assert_eq!(items_seasons["Items"].as_array().unwrap().len(), 1);
    assert_eq!(items_seasons["Items"][0]["Id"], season_id);
    assert_eq!(items_seasons["Items"][0]["SeriesId"], series_id);
    assert_eq!(items_seasons["Items"][0]["ParentId"], series_id);
    let Json(episodes) = get_show_children_from_all_servers(
        State(state.clone()),
        Preprocessed(request(
            &format!("/Shows/{series_id}/Episodes?SeasonId={season_id}"),
            &sessions,
        )),
    )
    .await
    .unwrap();
    assert_eq!(episodes["Items"].as_array().unwrap().len(), 1);
    assert_eq!(episodes["Items"][0]["SeasonId"], season_id);
    assert_eq!(episodes["Items"][0]["ParentId"], season_id);
    assert_eq!(episodes["Items"][0]["SeriesId"], series_id);
    assert_eq!(episodes["Items"][0]["MediaSourceCount"], 2);
    assert!(matches!(
        episodes["Items"][0]["Name"].as_str(),
        Some("Rebirth" | "Wiedergeburt")
    ));
}

#[tokio::test]
async fn client_pages_are_applied_after_scanning_capped_catalogs() {
    for (kind, endpoint) in [
        ("search", "/Items?SearchTerm=movie"),
        ("browse", "/Items?Recursive=true"),
        ("unmerged", "/Items?Recursive=true"),
        ("episodes", "/Shows/aggregate/Episodes?UserId=viewer"),
        ("seasons", "/Shows/aggregate/Seasons?UserId=viewer"),
    ] {
        let (state, _pool, sessions, upstreams) = setup().await;
        state.config.write().await.deduplicate_media = kind != "unmerged";
        for (server_index, upstream) in upstreams.iter().enumerate() {
            let upstream_path = match kind {
                "episodes" => "/Shows/show/Episodes",
                "seasons" => "/Shows/show/Seasons",
                _ => "/Items",
            };
            Mock::given(method("GET")).and(path(upstream_path))
                .respond_with(move |req: &wiremock::Request| {
                    let start = req.url.query_pairs().find(|(key, _)| key == "StartIndex")
                        .map(|(_, value)| value.parse::<usize>().unwrap()).unwrap_or(0);
                    let items = (start..(start + 2).min(5)).map(|index| {
                        let number = index + if kind == "unmerged" { server_index * 5 } else { 0 };
                        json!({"Id": format!("item-{index}"), "Name": format!("Item {number:02}"),
                            "Type": match kind { "episodes" => "Episode", "seasons" => "Season", _ => "Movie" },
                            "IndexNumber": number, "ParentIndexNumber": 1,
                            "ProviderIds": {"Tmdb": format!("{number}")}})
                    }).collect::<Vec<_>>();
                    ResponseTemplate::new(200).set_body_json(json!({"Items": items, "TotalRecordCount": 5, "StartIndex": start}))
                }).mount(upstream).await;
        }
        for (start, limit) in [(3, 2), (100, 2), (0, 0)] {
            let req = request(
                &format!(
                    "{}&StartIndex={start}&Limit={limit}&SortBy=Name",
                    endpoint.replace("aggregate", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                ),
                &sessions,
            );
            let targets = || {
                sessions
                    .iter()
                    .map(|(session, server)| CatalogFetchTarget {
                        session: session.clone(),
                        server: server.clone(),
                        parent_id: if matches!(kind, "episodes" | "seasons") {
                            Some("show".into())
                        } else {
                            None
                        },
                        resolved_parent_id: None,
                    })
                    .collect()
            };
            let Json(result) = match kind {
                "browse" => {
                    get_virtual_library_items(&state, req, "test:viewer".into(), targets(), 0).await
                }
                "episodes" | "seasons" => {
                    get_aggregate_show_items(
                        &state,
                        req,
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                        targets(),
                        0,
                    )
                    .await
                }
                _ => get_items_from_all_servers_preprocessed(&state, req).await,
            }
            .unwrap_or_else(|status| panic!("{kind}: {status}"));
            assert_eq!(
                result["TotalRecordCount"],
                if kind == "unmerged" { 10 } else { 5 },
                "{kind}"
            );
            assert_eq!(result["StartIndex"], start, "{kind}");
            let items = result["Items"].as_array().unwrap();
            if start == 3 {
                assert_eq!(items.len(), 2, "{kind}");
                assert!(
                    items[0]["Name"].as_str().unwrap().starts_with("Item 03"),
                    "{kind}"
                );
                assert!(
                    items[1]["Name"].as_str().unwrap().starts_with("Item 04"),
                    "{kind}"
                );
            } else {
                assert!(items.is_empty(), "{kind}");
            }
        }
    }
}

#[tokio::test]
async fn catalog_pagination_follows_backend_caps_and_detects_stalled_pages() {
    for (counted, stalled) in [(true, false), (false, false), (true, true)] {
        let (state, _pool, sessions, upstreams) = setup().await;
        for upstream in &upstreams {
            Mock::given(method("GET"))
                .and(path("/Items"))
                .respond_with(move |req: &wiremock::Request| {
                    let offset = if stalled { 0 } else {
                        req.url.query_pairs().find(|(key, _)| key == "StartIndex")
                            .unwrap().1.parse::<usize>().unwrap()
                    };
                    // This backend caps pages at two items regardless of Limit.
                    let items = (offset..(offset + 2).min(5)).map(|index| json!({
                        "Id": format!("item-{index}"), "Type": "Movie", "Name": format!("Movie {index}")
                    })).collect::<Vec<_>>();
                    ResponseTemplate::new(200).set_body_json(if counted {
                        json!({"Items": items, "TotalRecordCount": 5, "StartIndex": offset})
                    } else { json!(items) })
                })
                .mount(upstream).await;
        }
        let req = request("/Items?Recursive=true", &sessions);
        let policy = CatalogRequestPolicy::from_url(req.original_request.url());
        let targets = sessions
            .iter()
            .map(|(session, server)| CatalogFetchTarget {
                session: session.clone(),
                server: server.clone(),
                parent_id: None,
                resolved_parent_id: None,
            })
            .collect();
        let catalog = fetch_catalog(
            &state,
            &req.original_request,
            &policy,
            targets,
            FetchMode::VirtualLibrary,
            0,
        )
        .await;
        if stalled {
            assert!(matches!(catalog, Err(StatusCode::SERVICE_UNAVAILABLE)));
        } else {
            for result in catalog.unwrap().server_items {
                assert_eq!(result.server_items.response.len(), 5);
                assert_eq!(result.outcome, super::scan::FetchOutcome::Complete);
            }
        }
        let offsets = upstreams[0]
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .map(|req| {
                req.url
                    .query_pairs()
                    .find(|(key, _)| key == "StartIndex")
                    .unwrap()
                    .1
                    .parse::<usize>()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            offsets,
            if stalled {
                vec![0, 2]
            } else if counted {
                vec![0, 2, 4]
            } else {
                vec![0, 2, 4, 5]
            }
        );
    }
}

#[tokio::test]
async fn catalog_cache_contains_only_final_visible_items_and_titles() {
    let (state, _pool, sessions, upstreams) = setup().await;
    state.config.write().await.include_server_name_in_media = true;
    for (index, upstream) in upstreams.iter().enumerate() {
        Mock::given(method("GET"))
            .and(path("/Items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [
                    {"Id": format!("first-{index}"), "Type": "Movie", "Name": "A movie", "ProviderIds": {"Tmdb": "1"}},
                    {"Id": format!("second-{index}"), "Type": "Movie", "Name": "Z movie", "ProviderIds": {"Tmdb": "2"}}
                ], "TotalRecordCount": 2, "StartIndex": 0
            })))
            .mount(upstream).await;
    }
    let token = "catalog-client";
    let mut req = request("/Items?SearchTerm=movie&Limit=1", &sessions);
    req.original_request
        .headers_mut()
        .insert("X-Emby-Token", token.parse().unwrap());
    let Json(result) = get_items_from_all_servers_preprocessed(&state, req)
        .await
        .unwrap();
    assert_eq!(result["Items"].as_array().unwrap().len(), 1);
    let visible = &result["Items"][0];
    let cached = state
        .client_sessions
        .media_metadata(token, visible["Id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(cached["Name"], "A movie");
    for (index, (_, server)) in sessions.iter().enumerate() {
        for prefix in ["first", "second"] {
            let mapping = state
                .media_storage
                .get_or_create_media_mapping(&format!("{prefix}-{index}"), server)
                .await
                .unwrap();
            assert!(state
                .client_sessions
                .media_metadata(token, &mapping.virtual_media_id)
                .await
                .is_none());
        }
    }
    // An unmerged listing opts into presentation after translation. Its final
    // display title is also the title exposed to remote controls.
    state.config.write().await.deduplicate_media = false;
    let mut req = request("/Items?Limit=1", &sessions);
    req.original_request
        .headers_mut()
        .insert("X-Emby-Token", token.parse().unwrap());
    let Json(result) = get_items_from_all_servers_preprocessed(&state, req)
        .await
        .unwrap();
    let visible = &result["Items"][0];
    assert!(visible["Name"]
        .as_str()
        .unwrap()
        .starts_with("A movie [Server "));
    assert_eq!(
        state
            .client_sessions
            .media_metadata(token, visible["Id"].as_str().unwrap())
            .await
            .unwrap()["Name"],
        visible["Name"]
    );
}

#[tokio::test]
async fn search_first_show_children_merge_and_keep_viewer_scoped_parent_links() {
    let (state, _pool, sessions, upstreams) = setup().await;
    for (index, upstream) in upstreams.iter().enumerate() {
        Mock::given(method("GET")).and(path("/Items"))
            .and(query_param("SearchTerm", "show"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [
                    {"Id": "show", "Type": "Series", "Name": "Show", "ProviderIds": {"Tmdb": "42"}},
                    {"Id": "season", "Type": "Season", "SeriesId": "show", "ParentId": "show", "IndexNumber": 1,
                     "Name": if index == 0 { "Season 1" } else { "Staffel 1" }},
                    {"Id": "episode", "Type": "Episode", "SeriesId": "show", "SeasonId": "season", "ParentId": "season",
                     "IndexNumber": 1, "ParentIndexNumber": 1, "Name": if index == 0 { "Rebirth" } else { "Wiedergeburt" }}
                ], "TotalRecordCount": 3, "StartIndex": 0
            }))).mount(upstream).await;
    }
    // No browse/latest requests have established identities beforehand.
    let Json(result) = get_items_from_all_servers_preprocessed(
        &state,
        request("/Items?SearchTerm=show&Recursive=true", &sessions),
    )
    .await
    .unwrap();
    let items = result["Items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    let show = items.iter().find(|item| item["Type"] == "Series").unwrap();
    let season = items.iter().find(|item| item["Type"] == "Season").unwrap();
    let episode = items.iter().find(|item| item["Type"] == "Episode").unwrap();
    assert_eq!(season["SeriesId"], show["Id"]);
    assert_eq!(season["ParentId"], show["Id"]);
    assert_eq!(episode["SeriesId"], show["Id"]);
    assert_eq!(episode["SeasonId"], season["Id"]);
    assert_eq!(episode["ParentId"], season["Id"]);
    for (_, server) in &sessions {
        let mapping = state
            .media_storage
            .get_or_create_media_mapping("show", server)
            .await
            .unwrap();
        assert_eq!(
            state
                .media_storage
                .get_media_parent_group_id(&mapping.virtual_media_id, "viewer")
                .await
                .unwrap()
                .as_deref(),
            show["Id"].as_str()
        );
        assert!(state
            .media_storage
            .get_media_parent_group_id(&mapping.virtual_media_id, "other-viewer")
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn search_merges_movies_and_series_without_server_tags() {
    let (state, _pool, sessions, upstreams) = setup().await;
    state.config.write().await.include_server_name_in_media = true;
    for (index, upstream) in upstreams.iter().enumerate() {
        Mock::given(method("GET"))
            .and(path("/Items"))
            .and(query_param("SearchTerm", "Death"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [
                    {"Id": format!("movie-{index}"), "Type": "Movie", "Name": "Death Note [Live Action]", "ProviderIds": {"Tmdb": "13916"}},
                    {"Id": format!("series-{index}"), "Type": "Series", "Name": "Death Note", "ProviderIds": {"Tmdb": "13916"}}
                ],
                "TotalRecordCount": 2, "StartIndex": 0
            })))
            .mount(upstream).await;
    }
    let mut ids = None;
    for term_key in ["SearchTerm", "searchterm"] {
        let query = format!("/Items?{term_key}=Death&Recursive=true&Limit=10");
        let plan = resolve_catalog_plan(&state, &request(&query, &sessions))
            .await
            .unwrap();
        assert!(matches!(plan, CatalogPlan::Virtual { .. }));
    }
    for _ in 0..2 {
        let Json(result) = get_items_from_all_servers_preprocessed(
            &state,
            request("/Items?SearchTerm=Death&Recursive=true&Limit=10", &sessions),
        )
        .await
        .unwrap();
        assert_eq!(result["TotalRecordCount"], 2);
        let items = result["Items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["Name"], "Death Note");
        assert_eq!(items[1]["Name"], "Death Note [Live Action]");
        let current = (items[0]["Id"].clone(), items[1]["Id"].clone());
        if let Some(previous) = &ids {
            assert_eq!(previous, &current);
        }
        ids = Some(current);
    }
}

#[tokio::test]
async fn item_catalog_seasons_use_their_provider_matched_series_parent() {
    use super::{
        library_resolution::CatalogFetchTarget, media_reconciliation::get_virtual_library_items,
    };

    let (state, _pool, sessions, upstreams) = setup().await;
    let targets = || {
        sessions
            .iter()
            .map(|(session, server)| CatalogFetchTarget {
                session: session.clone(),
                server: server.clone(),
                parent_id: None,
                resolved_parent_id: None,
            })
            .collect()
    };
    for upstream in &upstreams {
        Mock::given(method("GET"))
            .and(path("/Items"))
            .and(query_param("IncludeItemTypes", "Series"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": "series", "Type": "Series", "ProviderIds": {"Imdb": "tt0877057"}}],
                "TotalRecordCount": 1, "StartIndex": 0
            })))
            .mount(upstream)
            .await;
    }
    let Json(series) = get_virtual_library_items(
        &state,
        request("/Items?IncludeItemTypes=Series", &sessions),
        "configured:shows:viewer".into(),
        targets(),
        0,
    )
    .await
    .unwrap();
    assert_eq!(series["Items"].as_array().unwrap().len(), 1);

    for (index, upstream) in upstreams.iter().enumerate() {
        Mock::given(method("GET"))
            .and(path("/Items"))
            .and(query_param("IncludeItemTypes", "Season"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [
                    {"Id": "specials", "Type": "Season", "SeriesId": "series", "IndexNumber": 0,
                     "Name": if index == 0 { "Specials" } else { "Extras" }},
                    {"Id": "season", "Type": "Season", "SeriesId": "series", "IndexNumber": 1,
                     "Name": if index == 0 { "Season 1" } else { "Staffel 1" }}
                ],
                "TotalRecordCount": 2, "StartIndex": 0
            })))
            .mount(upstream)
            .await;
    }
    let Json(seasons) = get_virtual_library_items(
        &state,
        request("/Items?IncludeItemTypes=Season", &sessions),
        "configured:shows:viewer".into(),
        targets(),
        0,
    )
    .await
    .unwrap();
    let seasons = seasons["Items"].as_array().unwrap();
    assert_eq!(seasons.len(), 2);
    for season in seasons {
        let members = state
            .media_storage
            .get_media_version_members_by_virtual_id(season["Id"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(members.len(), 2);
    }
}

#[tokio::test]
async fn first_recursive_catalog_merges_series_and_numbered_seasons_together() {
    use super::{
        library_resolution::CatalogFetchTarget, media_reconciliation::get_virtual_library_items,
    };
    let (state, _pool, sessions, upstreams) = setup().await;
    for (index, upstream) in upstreams.iter().enumerate() {
        Mock::given(method("GET")).and(path("/Items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [
                    {"Id": format!("series-{index}"), "Type": "Series", "ProviderIds": {"Tmdb": "13916"}},
                    {"Id": format!("season-{index}"), "Type": "Season", "SeriesId": format!("series-{index}"),
                     "ParentId": format!("series-{index}"), "IndexNumber": 1,
                     "Name": if index == 0 { "Season 1" } else { "Staffel 1" }}
                ], "TotalRecordCount": 2, "StartIndex": 0
            }))).mount(upstream).await;
    }
    let mut previous_ids = None;
    for _ in 0..2 {
        let targets = sessions
            .iter()
            .map(|(session, server)| CatalogFetchTarget {
                session: session.clone(),
                server: server.clone(),
                parent_id: None,
                resolved_parent_id: None,
            })
            .collect();
        let Json(response) = get_virtual_library_items(
            &state,
            request("/Items?Recursive=true", &sessions),
            "configured:shows:viewer".into(),
            targets,
            0,
        )
        .await
        .unwrap();
        let items = response["Items"].as_array().unwrap();
        assert_eq!(
            items.len(),
            2,
            "the first response must merge both series and season"
        );
        let show = items.iter().find(|item| item["Type"] == "Series").unwrap();
        let season = items.iter().find(|item| item["Type"] == "Season").unwrap();
        assert_eq!(season["SeriesId"], show["Id"]);
        assert_eq!(season["ParentId"], show["Id"]);
        let ids = (show["Id"].clone(), season["Id"].clone());
        if let Some(previous) = &previous_ids {
            assert_eq!(&ids, previous);
        }
        previous_ids = Some(ids);
    }
    // The additive parent pass must not prevent the final complete inventory
    // from removing a season that has disappeared from both backends.
    state.catalog_snapshots.expire();
    for (index, upstream) in upstreams.iter().enumerate() {
        upstream.reset().await;
        Mock::given(method("GET")).and(path("/Items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [
                    {"Id": format!("series-{index}"), "Type": "Series", "ProviderIds": {"Tmdb": "13916"}},
                    {"Id": format!("new-season-{index}"), "Type": "Season", "SeriesId": format!("series-{index}"),
                     "IndexNumber": 2, "Name": "Season 2"}
                ], "TotalRecordCount": 2, "StartIndex": 0
            }))).mount(upstream).await;
    }
    let targets = sessions
        .iter()
        .map(|(session, server)| CatalogFetchTarget {
            session: session.clone(),
            server: server.clone(),
            parent_id: None,
            resolved_parent_id: None,
        })
        .collect();
    let Json(response) = get_virtual_library_items(
        &state,
        request("/Items?Recursive=true", &sessions),
        "configured:shows:viewer".into(),
        targets,
        0,
    )
    .await
    .unwrap();
    assert_eq!(response["Items"].as_array().unwrap().len(), 2);
    let removed_season = previous_ids.unwrap().1;
    assert!(state
        .media_storage
        .get_media_version_members_by_virtual_id(removed_season.as_str().unwrap())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn numbered_seasons_with_same_server_duplicates_are_not_hidden() {
    use super::{
        library_resolution::CatalogFetchTarget, media_reconciliation::get_aggregate_show_items,
    };
    let (state, _pool, sessions, upstreams) = setup().await;
    let aggregate = "00000000000000000000000000000100";
    for (index, upstream) in upstreams.iter().enumerate() {
        let mut seasons =
            vec![json!({"Id": "season", "Type": "Season", "IndexNumber": 1, "Name": "Season 1"})];
        if index == 0 {
            seasons.push(json!({"Id": "duplicate", "Type": "Season", "IndexNumber": 1, "Name": "Season 1 copy"}));
        }
        Mock::given(method("GET"))
            .and(path("/Shows/series/Seasons"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "TotalRecordCount": seasons.len(), "StartIndex": 0, "Items": seasons
            })))
            .mount(upstream)
            .await;
    }
    let targets = sessions
        .iter()
        .map(|(session, server)| CatalogFetchTarget {
            session: session.clone(),
            server: server.clone(),
            parent_id: Some("series".into()),
            resolved_parent_id: Some(aggregate.into()),
        })
        .collect();
    let Json(response) = get_aggregate_show_items(
        &state,
        request(&format!("/Shows/{aggregate}/Seasons"), &sessions),
        aggregate.into(),
        targets,
        0,
    )
    .await
    .unwrap();
    let seasons = response["Items"].as_array().unwrap();
    assert_eq!(seasons.len(), 3);
    for season in seasons {
        assert!(state
            .media_storage
            .get_media_version_group(season["Id"].as_str().unwrap())
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn show_seasons_with_reused_or_conflicting_provider_ids_merge_by_number() {
    check_show_season_navigation(true).await;
}

async fn check_show_season_navigation(with_provider_ids: bool) {
    use super::{
        library_resolution::CatalogFetchTarget, media_reconciliation::get_aggregate_show_items,
    };
    use wiremock::matchers::{path, query_param};

    let (state, _pool, sessions, upstreams) = setup().await;
    let aggregate = "00000000000000000000000000000100";
    for (server_index, (upstream, available_seasons)) in [
        (&upstreams[0], &[(1, "season-one"), (2, "season-two")][..]),
        (&upstreams[1], &[(1, "season-one")][..]),
    ]
    .into_iter()
    .enumerate()
    {
        let seasons = available_seasons
            .iter()
            .map(|&(number, id)| {
                json!({
                    "Id": id, "Type": "Season", "SeriesId": "series",
                    "Name": if server_index == 0 { format!("Season {number}") } else { format!("Staffel {number}") },
                    "IndexNumber": number,
                    "ProviderIds": if with_provider_ids { json!({"Tmdb": format!("show-{server_index}")}) } else { json!({}) }
                })
            })
            .collect::<Vec<_>>();
        Mock::given(method("GET"))
            .and(path("/Shows/series/Seasons"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "TotalRecordCount": seasons.len(), "StartIndex": 0, "Items": seasons
            })))
            .expect(1)
            .mount(upstream)
            .await;
        for &(number, season) in available_seasons {
            Mock::given(method("GET"))
                .and(path("/Shows/series/Episodes"))
                .and(query_param("seasonId", season))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "TotalRecordCount": 1, "StartIndex": 0, "Items": [{
                        "Id": format!("episode-{number}"), "Type": "Episode",
                        "SeriesId": "series", "SeasonId": season,
                        "ParentIndexNumber": number, "IndexNumber": 2,
                        "ProviderIds": {"Tmdb": format!("20{number}")}
                    }]
                })))
                .expect(1)
                .mount(upstream)
                .await;
        }
    }
    let targets = || {
        sessions
            .iter()
            .map(|(session, server)| CatalogFetchTarget {
                session: session.clone(),
                server: server.clone(),
                parent_id: Some("series".into()),
                resolved_parent_id: Some(aggregate.into()),
            })
            .collect()
    };
    let Json(seasons) = get_aggregate_show_items(&state,
        request(&format!("/Shows/{aggregate}/Seasons?userId=viewer&Fields=ItemCounts,PrimaryImageAspectRatio,CanDelete,MediaSourceCount"), &sessions),
        aggregate.into(), targets(), 0).await.unwrap();
    let seasons = seasons["Items"].as_array().unwrap();
    assert_eq!(seasons.len(), 2);
    for (index, season) in seasons.iter().enumerate() {
        assert_eq!(season["SeriesId"], aggregate);
        assert_eq!(season["IndexNumber"], index + 1);
        let Json(episodes) = get_aggregate_show_items(
            &state,
            request(
                &format!(
                    "/Shows/{}/Episodes?seasonId={}",
                    season["SeriesId"].as_str().unwrap(),
                    season["Id"].as_str().unwrap()
                ),
                &sessions,
            ),
            aggregate.into(),
            targets(),
            0,
        )
        .await
        .unwrap();
        let episodes = episodes["Items"].as_array().unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0]["SeriesId"], aggregate);
        assert_eq!(episodes[0]["SeasonId"], season["Id"]);
        assert_eq!(episodes[0]["ParentIndexNumber"], index + 1);
        if index == 0 {
            assert_eq!(episodes[0]["MediaSourceCount"], 2);
        }
    }
    // No stray request carrying a virtual season was sent to the other server.
    assert_eq!(upstreams[0].received_requests().await.unwrap().len(), 3);
    assert_eq!(upstreams[1].received_requests().await.unwrap().len(), 2);

    let member = state
        .media_storage
        .get_media_mapping_by_original("season-one", sessions[0].1.id)
        .await
        .unwrap()
        .unwrap();
    let mut detail = json!({
        "Id": "selected-episode", "Type": "Episode", "SeriesId": aggregate,
        "SeasonId": member.virtual_media_id, "ParentId": member.virtual_media_id,
        "MediaSources": [{"Id": "selected-source"}]
    });
    let preprocessed = request("/Items/selected-episode", &sessions);
    crate::handlers::media_versions::merge_media_detail(
        &state,
        crate::handlers::media_versions::DetailMergeContext {
            requested_item_id: "selected-episode",
            selected_group: None,
            base_server: &sessions[0].1,
            auth: &None,
            access_scope: preprocessed.access_scope.as_ref(),
            sessions: Some(&sessions),
            original_request: &preprocessed.original_request,
            source_generation: 0,
        },
        None,
        &mut detail,
    )
    .await
    .unwrap();
    assert_eq!(detail["Id"], "selected-episode");
    assert_eq!(detail["MediaSources"][0]["Id"], "selected-source");
    assert_eq!(detail["SeasonId"], seasons[0]["Id"]);
    assert_eq!(detail["ParentId"], seasons[0]["Id"]);
}

#[tokio::test]
async fn latest_routes_collapse_before_sort_and_limit_and_retain_partial_identity() {
    let (state, pool, sessions, upstreams) = setup().await;
    for upstream in &upstreams {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(latest_items()))
            .mount(upstream)
            .await;
    }
    let mut stable_ids = Vec::new();
    for path in [
        "/Users/viewer/Items/Latest?Limit=2&Fields=Path",
        "/Items/Latest?UserId=viewer&Limit=2&Fields=Path",
    ] {
        let Json(response) = get_items_from_all_servers_if_not_restricted(
            State(state.clone()),
            Preprocessed(request(path, &sessions)),
        )
        .await
        .unwrap();
        let items = response
            .as_array()
            .expect("latest must remain a bare array");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["Type"], "Series");
        assert_eq!(items[1]["Type"], "Movie");
        let ids = items
            .iter()
            .map(|item| item["Id"].clone())
            .collect::<Vec<_>>();
        if stable_ids.is_empty() {
            stable_ids = ids;
        } else {
            assert_eq!(ids, stable_ids);
        }
    }
    for upstream in &upstreams {
        for request in upstream.received_requests().await.unwrap() {
            let fields = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "Fields")
                .unwrap()
                .1
                .into_owned();
            for field in ["Path", "ProviderIds", "DateCreated"] {
                assert!(fields.split(',').any(|value| value == field), "{fields}");
            }
            assert!(!request
                .url
                .query_pairs()
                .any(|(key, _)| key == "StartIndex"));
        }
    }
    let Json(response) = get_items_from_all_servers_preprocessed(
        &state,
        request("/Items/Latest?StartIndex=1&Limit=1", &sessions),
    )
    .await
    .unwrap();
    assert_eq!(response[0]["Id"], stable_ids[1]);

    // A later window and an unavailable server must not remove older sightings.
    upstreams[0].reset().await;
    upstreams[1].reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([latest_items()[1].clone()])))
        .mount(&upstreams[0])
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&upstreams[1])
        .await;
    let degraded = get_items_from_all_servers_preprocessed(
        &state,
        request("/Items/Latest?Limit=1", &sessions),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(degraded[0]["Id"], stable_ids[0]);
    for active_sessions in [&sessions[..1]] {
        let Json(response) = get_items_from_all_servers_preprocessed(
            &state,
            request("/Items/Latest?Limit=1", active_sessions),
        )
        .await
        .unwrap();
        assert_eq!(response[0]["Id"], stable_ids[0]);
    }
    let (sightings,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM movie_catalog_sightings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sightings, 6);
    let (scopes,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM movie_catalog_scopes WHERE scope_key LIKE 'latest:%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(scopes, 1);
}

#[tokio::test]
async fn latest_virtual_parent_reuses_browse_scope_and_member_routes() {
    let (state, pool, sessions, upstreams) = setup().await;
    let group = state
        .virtual_library_service
        .create_group("Movies")
        .await
        .unwrap();
    for (index, (_, server)) in sessions.iter().enumerate() {
        state
            .virtual_library_service
            .add_member(
                &group.virtual_id,
                server.id,
                &format!("library{index}"),
                "Movies",
                "movies",
            )
            .await
            .unwrap();
        Mock::given(method("GET"))
            .and(move |request: &wiremock::Request| {
                request.url.query_pairs().any(|(key, value)| {
                    key.eq_ignore_ascii_case("ParentId") && value == format!("library{index}")
                })
            })
            .respond_with(move |request: &wiremock::Request| {
                let fields = request
                    .url
                    .query_pairs()
                    .filter(|(key, _)| key.eq_ignore_ascii_case("Fields"))
                    .collect::<Vec<_>>();
                // Jellyfin does not reliably bind repeated comma-separated field lists.
                if fields.len() != 1 || fields[0].0 != "Fields" {
                    return ResponseTemplate::new(400);
                }
                let mut values = fields[0].1.split(',').collect::<Vec<_>>();
                values.sort_unstable();
                let mut expected = vec![
                    "DateCreated",
                    "Path",
                    "PrimaryImageAspectRatio",
                    "ProviderIds",
                ];
                if request.url.path().eq_ignore_ascii_case("/Items") {
                    expected.push("SortName");
                }
                if values != expected {
                    return ResponseTemplate::new(400);
                }
                if request
                    .url
                    .query_pairs()
                    .any(|(key, value)| key == "StartIndex" && value != "0")
                {
                    return ResponseTemplate::new(200).set_body_json(json!([]));
                }
                let mut items = latest_items();
                for item in items.as_array_mut().unwrap() {
                    item["Path"] = json!(format!(
                        "/server{index}/{}.mkv",
                        item["Id"].as_str().unwrap()
                    ));
                }
                ResponseTemplate::new(200).set_body_json(items)
            })
            .expect(4)
            .mount(&upstreams[index])
            .await;
    }
    let mut scope_key = None;
    let mut ids = None;
    for endpoint in ["/Items", "/Users/viewer/Items/Latest", "/Items/Latest"] {
        let fields = if endpoint == "/Items" {
            "Fields=PrimaryImageAspectRatio,Path,DateCreated"
        } else {
            "fields=PrimaryImageAspectRatio&fields=Path"
        };
        let path = format!(
            "{endpoint}?userId=viewer&parentId={}&limit=10&{fields}&imageTypeLimit=1&enableImageTypes=Primary&enableImageTypes=Thumb",
            group.virtual_id
        );
        let preprocessed = request(&path, &sessions);
        let CatalogPlan::Virtual {
            catalog_scope_key,
            targets,
            ..
        } = resolve_catalog_plan(&state, &preprocessed).await.unwrap()
        else {
            panic!("virtual parent must fan out");
        };
        assert_eq!(targets.len(), 2);
        if let Some(expected) = &scope_key {
            assert_eq!(&catalog_scope_key, expected);
        } else {
            scope_key = Some(catalog_scope_key);
        }
        let Json(response) = get_items_from_all_servers_preprocessed(&state, preprocessed)
            .await
            .unwrap_or_else(|status| panic!("{path}: {status}"));
        let mut current_ids = response
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["Id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        current_ids.sort();
        assert_eq!(current_ids.len(), 4);
        if let Some(expected) = &ids {
            assert_eq!(&current_ids, expected);
        } else {
            ids = Some(current_ids);
        }
    }
    let (scopes,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM movie_catalog_scopes WHERE scope_key LIKE 'latest:%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(scopes, 0);
    let mapping = state
        .media_storage
        .get_or_create_media_mapping("unmerged-library", &sessions[0].1)
        .await
        .unwrap();
    assert!(matches!(
        resolve_catalog_plan(
            &state,
            &request(
                &format!("/Items/Latest?ParentId={}", mapping.virtual_media_id),
                &sessions,
            )
        )
        .await
        .unwrap(),
        CatalogPlan::SingleServer
    ));
}

#[tokio::test]
async fn latest_does_not_collapse_same_server_ambiguity() {
    let (state, _, sessions, upstreams) = setup().await;
    let movie = latest_items()[0].clone();
    let mut alternate = movie.clone();
    alternate["Id"] = json!("alternate");
    for (upstream, items) in upstreams
        .iter()
        .zip([json!([movie.clone(), alternate]), json!([movie])])
    {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(items))
            .mount(upstream)
            .await;
    }
    let Json(response) = get_items_from_all_servers_preprocessed(
        &state,
        request("/Items/Latest?Limit=10", &sessions),
    )
    .await
    .unwrap();
    assert_eq!(response.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn latest_preserves_unknown_identity_and_disabled_behavior() {
    let (state, _, sessions, upstreams) = setup().await;
    for upstream in &upstreams {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(latest_items()))
            .mount(upstream)
            .await;
    }
    for (enabled, expected) in [(true, 4), (false, 6)] {
        state.config.write().await.deduplicate_media = enabled;
        let Json(response) = get_items_from_all_servers_preprocessed(
            &state,
            request("/Items/Latest?Limit=10", &sessions),
        )
        .await
        .unwrap();
        assert_eq!(response.as_array().unwrap().len(), expected);
    }
    assert!(matches!(
        resolve_catalog_plan(&state, &request("/Items/Latest", &sessions))
            .await
            .unwrap(),
        CatalogPlan::Interleaved(_)
    ));
    state.config.write().await.deduplicate_media = true;
    for path in ["/Items", "/UserItems/Resume", "/Items/Suggestions"] {
        assert!(matches!(
            resolve_catalog_plan(&state, &request(path, &sessions))
                .await
                .unwrap(),
            CatalogPlan::Interleaved(_)
        ));
    }
}
