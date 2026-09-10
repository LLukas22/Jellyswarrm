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
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

async fn setup() -> (
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

fn request(path: &str, sessions: &[(AuthorizationSession, Server)]) -> PreprocessedRequest {
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

#[tokio::test]
async fn show_navigation_preserves_parents_and_skips_servers_without_the_season() {
    use super::{
        library_resolution::CatalogFetchTarget, media_reconciliation::get_aggregate_show_items,
    };
    use wiremock::matchers::{path, query_param};

    let (state, _pool, sessions, upstreams) = setup().await;
    let aggregate = "00000000000000000000000000000100";
    for (upstream, available_seasons) in [
        (&upstreams[0], &[(1, "season-one"), (2, "season-two")][..]),
        (&upstreams[1], &[(1, "season-one")][..]),
    ] {
        let seasons = available_seasons
            .iter()
            .map(|&(number, id)| {
                json!({
                    "Id": id, "Type": "Season", "SeriesId": "series",
                    "IndexNumber": number, "ProviderIds": {"Tmdb": format!("10{number}")}
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
    for active_sessions in [&sessions[..], &sessions[..1]] {
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
                if values
                    != [
                        "DateCreated",
                        "Path",
                        "PrimaryImageAspectRatio",
                        "ProviderIds",
                    ]
                {
                    return ResponseTemplate::new(400);
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
            .expect(3)
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
