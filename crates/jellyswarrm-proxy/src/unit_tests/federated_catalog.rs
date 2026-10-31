//! Federated catalog regressions for merging, pagination, scoping, and backend failures.

use super::media_reconciliation::{get_aggregate_show_items, get_virtual_library_items};
use super::*;
use crate::application::catalog::library_resolution::ResolvedCatalogParent;
use crate::{
    auth::service::AuthorizationSession, config::AppConfig,
    libraries::service::VirtualLibraryAccessScope, persistence::MIGRATOR, protocol::device::Device,
    servers::storage::Server, test_support::MediaFixture,
};
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

pub(crate) async fn setup() -> (
    MediaFixture,
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
    let state = MediaFixture::new(
        pool.clone(),
        AppConfig {
            deduplicate_media: true,
            merge_libraries: false,
            ..AppConfig::default()
        },
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

pub(crate) async fn fixture_member_parent_ids(
    state: &MediaFixture,
    sessions: &[(AuthorizationSession, Server)],
    original_parent_id: &str,
) -> std::collections::HashMap<crate::domain::ids::ServerId, String> {
    let mut ids = std::collections::HashMap::new();
    for (_, server) in sessions {
        let mapping = state
            .media_storage
            .get_or_create_media_mapping(original_parent_id, server)
            .await
            .unwrap();
        ids.insert(server.id, mapping.virtual_media_id);
    }
    ids
}

pub(crate) fn request(path: &str, sessions: &[(AuthorizationSession, Server)]) -> OperationRequest {
    let original_request = reqwest::Request::new(
        reqwest::Method::GET,
        url::Url::parse(&format!("http://localhost{path}")).unwrap(),
    );
    let context = crate::application::request::context::RequestContext::new(
        &original_request,
        None,
        None,
        Some(sessions.to_vec()),
        Some(VirtualLibraryAccessScope::new(
            "viewer",
            sessions.iter().map(|(_, server)| server.id),
        )),
    );
    OperationRequest {
        original_request,
        context,
        body_analysis: None,
    }
}

#[tokio::test]
async fn request_independent_refresh_shares_paging_reconciliation_and_interactive_identity() {
    use crate::application::catalog::refresh::{refresh, CatalogRefresh};
    let (state, _pool, sessions, upstreams) = setup().await;
    let library = state
        .virtual_library_service
        .create_group("Refresh movies")
        .await
        .unwrap();
    for (index, (_, server)) in sessions.iter().enumerate() {
        // Same-backend ID collision also applies to refresh input: only virtual
        // member IDs may enter the shared request construction stage.
        let decoy = state
            .media_storage
            .get_or_create_media_mapping("wrong-parent", server)
            .await
            .unwrap();
        let original_parent = decoy.virtual_media_id;
        state
            .virtual_library_service
            .add_member(
                &library.virtual_id,
                server.id,
                &original_parent,
                "Movies",
                "movies",
            )
            .await
            .unwrap();
        Mock::given(method("GET")).and(path("/Items"))
            .and(query_param("ParentId", original_parent))
            .and(query_param("Recursive", "true"))
            .respond_with(move |request: &wiremock::Request| {
                let start = request.url.query_pairs().find(|(key, _)| key == "StartIndex").unwrap().1.parse::<usize>().unwrap();
                let items = if start < 2 { vec![json!({"Id": format!("refresh-{index}-{start}"), "Type":"Movie", "Name":format!("Movie {start}"), "ProviderIds":{"Tmdb":format!("{}", 8000 + start)}})] } else { vec![] };
                ResponseTemplate::new(200).set_body_json(json!({"Items":items,"StartIndex":start,"TotalRecordCount":2}))
            }).mount(&upstreams[index]).await;
    }
    let make_input = || CatalogRefresh {
        library_id: library.virtual_id.clone(),
        access_scope: VirtualLibraryAccessScope::new(
            "viewer",
            sessions.iter().map(|(_, server)| server.id),
        ),
        sessions: sessions
            .iter()
            .map(|(session, _)| session.clone())
            .collect(),
    };
    // No reqwest/http request is constructed for either refresh invocation.
    let first = refresh(&state, make_input()).await.unwrap();
    assert!(first.report.is_complete_inventory());
    assert_eq!(first.report.outcomes().len(), sessions.len());
    assert_eq!(first.response["Items"].as_array().unwrap().len(), 2);
    for upstream in &upstreams {
        assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    }
    let interactive = get_catalog(
        &state,
        request(
            &format!(
                "/Items?ParentId={}&Recursive=true&IncludeItemTypes=Movie,Series,Season,Episode",
                library.virtual_id
            ),
            &sessions,
        ),
    )
    .await
    .unwrap();
    assert_eq!(interactive, first.response);
    let first_ids = first.response["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Id"].clone())
        .collect::<Vec<_>>();
    // Losing a source does not turn a partial refresh into a pruning inventory.
    upstreams[1].reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&upstreams[1])
        .await;
    let partial = refresh(&state, make_input()).await.unwrap();
    assert!(!partial.report.is_complete_inventory());
    let ids = partial.response["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(ids, first_ids);
    for id in first_ids {
        let group = state
            .media_storage
            .get_media_version_group(id.as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state
                .media_storage
                .get_media_version_members(group.id)
                .await
                .unwrap()
                .len(),
            2
        );
    }
    // The earlier interactive response was materialized. A refresh must fence
    // it so the next interactive request does not silently return stale cache.
    let before = upstreams[0].received_requests().await.unwrap().len();
    let _ = get_catalog(
        &state,
        request(
            &format!(
                "/Items?ParentId={}&Recursive=true&IncludeItemTypes=Movie,Series,Season,Episode",
                library.virtual_id
            ),
            &sessions,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        upstreams[0].received_requests().await.unwrap().len(),
        before + 2
    );
}

#[tokio::test]
async fn request_independent_refresh_rejects_foreign_viewer_credentials_before_traffic() {
    use crate::application::{
        catalog::refresh::{refresh, CatalogRefresh},
        error::ApplicationError,
    };
    let (state, _pool, sessions, upstreams) = setup().await;
    let result = refresh(
        &state,
        CatalogRefresh {
            library_id: "unresolved".into(),
            access_scope: VirtualLibraryAccessScope::new(
                "other-viewer",
                sessions.iter().map(|(_, server)| server.id),
            ),
            sessions: sessions
                .iter()
                .map(|(session, _)| session.clone())
                .collect(),
        },
    )
    .await;
    assert!(matches!(result, Err(ApplicationError::AccessDenied)));
    for upstream in upstreams {
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn catalog_service_boundaries_preserve_source_and_storage_causes() {
    use crate::application::{
        catalog::{filters::get_filters, get_catalog},
        request::input::OperationRequest,
    };
    let (state, pool, sessions, upstreams) = setup().await;
    let mut ids = Vec::new();
    for (_, server) in &sessions {
        ids.push(
            state
                .media_storage
                .get_or_create_media_mapping("movie", server)
                .await
                .unwrap()
                .virtual_media_id,
        );
    }
    for upstream in &upstreams {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("private-invalid-catalog"))
            .mount(upstream)
            .await;
    }
    for path in [
        "/Users/viewer/Views".to_string(),
        "/Items/Filters".to_string(),
        format!("/Items?Ids={}", ids.join(",")),
    ] {
        let input: OperationRequest = request(&path, &sessions);
        let result = if path.ends_with("/Filters") {
            get_filters(&state, input).await
        } else {
            get_catalog(&state, input).await
        };
        let error = result.unwrap_err();

        let mut cause: &(dyn std::error::Error + 'static) = &error;
        while !cause.is::<serde_json::Error>() {
            cause = cause
                .source()
                .expect("source JSON cause must survive catalog boundary");
        }
        assert!(!format!("{error:?}").contains("private-invalid-catalog"));
    }
    pool.close().await;
    for path in [
        "/Items?ParentId=library",
        "/Items/Filters?ParentId=library",
        "/Items?Ids=unknown",
    ] {
        let input: OperationRequest = request(path, &sessions);
        let result = if path.contains("/Filters") {
            get_filters(&state, input).await
        } else {
            get_catalog(&state, input).await
        };
        let error = result.unwrap_err();

        let mut cause: &(dyn std::error::Error + 'static) = &error;
        while !cause.is::<sqlx::Error>() {
            cause = cause
                .source()
                .expect("storage cause must survive catalog boundary");
        }
    }
}

#[tokio::test]
async fn catalog_query_is_always_derived_from_the_authoritative_wire_request() {
    let (_state, _pool, sessions, _upstreams) = setup().await;
    let mut input: crate::application::request::input::OperationRequest =
        request("/Items?ParentId=before&Limit=10", &sessions);
    *input.original_request.url_mut() = "http://localhost/Items?ParentId=after&Limit=3"
        .parse()
        .unwrap();
    assert_eq!(
        input.catalog_query().identity(),
        crate::application::catalog::query::CatalogQuery::from_url(input.original_request.url())
            .identity()
    );
    assert!(input.catalog_query().identity().contains("ParentId=after"));
    assert!(!input.catalog_query().identity().contains("before"));
}

fn latest_items() -> Value {
    json!([
        {"Id":"movie", "Name":"Movie", "Type":"Movie", "ProviderIds":{"Tmdb":"42"}, "DateCreated":"2026-01-01T00:00:00Z"},
        {"Id":"series", "Name":"Show", "Type":"Series", "ProviderIds":{"Tmdb":"42"}, "DateCreated":"2026-02-01T00:00:00Z"},
        {"Id":"unknown", "Name":"Unidentified", "Type":"Movie", "DateCreated":"2025-01-01T00:00:00Z"}
    ])
}

pub(crate) fn library_items(count: usize) -> Vec<Value> {
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
        let response = get_catalog(
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
        let result = get_catalog(&state, request("/Users/viewer/Views", &sessions[..1])).await;
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
        .url_selection
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

    let preprocessed = request(
        &format!("{}?{}", url.path(), url.query().unwrap()),
        &sessions,
    );
    let response = get_live_tv_programs(&state, preprocessed).await.unwrap();
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
        .url_selection
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

    let preprocessed = request(
        &format!("{}?{}", url.path(), url.query().unwrap()),
        &sessions,
    );
    let response = get_live_tv_programs(&state, preprocessed).await.unwrap();
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
    let response = get_live_tv_programs(&state, request(&path, &sessions))
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
            .url_selection
            .server_from_client_url(&timer_url, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(server.id, sessions[index].1.id);
        state
            .processors
            .url_processor
            .client_to_server_url(&mut timer_url, None, None, server.id)
            .await
            .unwrap();
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
    let shows = get_catalog(
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
        let seasons = get_show_children(&state, request(&path, &sessions))
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
    let items_seasons = get_catalog(
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
    let episodes = get_show_children(
        &state,
        request(
            &format!("/Shows/{series_id}/Episodes?SeasonId={season_id}"),
            &sessions,
        ),
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
async fn episode_playback_queue_spans_servers_and_applies_start_before_pagination() {
    let (state, _pool, sessions, upstreams) = setup().await;
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
                "library",
                "Shows",
                "tvshows",
            )
            .await
            .unwrap();
        Mock::given(method("GET"))
            .and(path("/Items"))
            .and(query_param("ParentId", "library"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": "series", "Type": "Series", "ProviderIds": {"Tvdb": "42"}}],
                "TotalRecordCount": 1, "StartIndex": 0
            })))
            .mount(&upstreams[index])
            .await;
        let numbers = if index == 0 {
            vec![1, 2]
        } else {
            vec![2, 3, 4]
        };
        let episodes = numbers.into_iter().map(|number| json!({
            "Id": format!("episode-{number}"), "Type": "Episode", "SeriesId": "series",
            "Name": format!("Episode {number}"),
            "ParentIndexNumber": if number == 4 { 2 } else { 1 },
            "IndexNumber": if number == 4 { 1 } else { number },
            "UserData": {"Played": false, "PlaybackPositionTicks": 0, "PlayCount": 0,
                "IsFavorite": false, "Key": format!("episode-{number}"), "ItemId": format!("episode-{number}")}
        })).collect::<Vec<_>>();
        Mock::given(method("GET"))
            .and(path("/Shows/series/Episodes"))
            .respond_with(move |request: &wiremock::Request| {
                // A backend cannot resolve another server's anchor (nor a
                // proxy aggregate ID). Model that failure rather than ignoring
                // the filter, so this regression fails if it is forwarded.
                let items = if request
                    .url
                    .query_pairs()
                    .any(|(key, _)| key.eq_ignore_ascii_case("StartItemId"))
                {
                    vec![]
                } else {
                    episodes.clone()
                };
                ResponseTemplate::new(200).set_body_json(json!({
                    "TotalRecordCount": items.len(), "StartIndex": 0, "Items": items
                }))
            })
            .mount(&upstreams[index])
            .await;
    }
    let shows = get_catalog(
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
    let series_id = shows["Items"][0]["Id"].as_str().unwrap();
    let endpoint = format!("/Shows/{series_id}/Episodes");
    let listing = get_show_children(&state, request(&endpoint, &sessions))
        .await
        .unwrap();
    let ids = listing["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 4);
    let alternate = state
        .media_storage
        .get_or_create_media_mapping("episode-2", &sessions[1].1)
        .await
        .unwrap()
        .virtual_media_id;
    let formatted_anchor = uuid::Uuid::parse_str(&ids[1])
        .unwrap()
        .hyphenated()
        .to_string()
        .to_uppercase();
    for (anchor, key, offset, limit, expected, total) in [
        (ids[0].as_str(), "StartItemId", 0, 100, &ids[..], 4),
        (ids[1].as_str(), "startItemId", 0, 100, &ids[1..], 3),
        (alternate.as_str(), "STARTITEMID", 0, 100, &ids[1..], 3),
        (
            formatted_anchor.as_str(),
            "StartItemId",
            0,
            100,
            &ids[1..],
            3,
        ),
        (ids[0].as_str(), "StartItemId", 1, 1, &ids[1..2], 4),
        // Same snapshot, different client page: do not apply the anchor twice.
        (ids[0].as_str(), "StartItemId", 2, 2, &ids[2..], 4),
        (ids[2].as_str(), "StartItemId", 0, 100, &ids[2..], 2),
        (ids[3].as_str(), "StartItemId", 0, 100, &ids[3..], 1),
    ] {
        let queue = get_show_children(
            &state,
            request(&format!("{endpoint}?{key}={anchor}&StartIndex={offset}&Limit={limit}&IsMissing=false&IsVirtualUnaired=false&Fields=MediaSources"), &sessions),
        ).await.unwrap();
        let actual = queue["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["Id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "anchor={anchor}, offset={offset}");
        assert_eq!(queue["TotalRecordCount"], total);
        assert_eq!(queue["StartIndex"], offset);
        assert!(queue["Items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["SeriesId"] == series_id));
    }
    let requests = upstreams[1].received_requests().await.unwrap();
    assert!(requests
        .iter()
        .filter(|request| request.url.path().ends_with("/Episodes"))
        .all(|request| !request
            .url
            .query_pairs()
            .any(|(key, _)| key.eq_ignore_ascii_case("StartItemId"))));
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
        let member_parent_ids = if matches!(kind, "episodes" | "seasons") {
            fixture_member_parent_ids(&state, &sessions, "show").await
        } else {
            std::collections::HashMap::new()
        };
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
                        parent: if matches!(kind, "episodes" | "seasons") {
                            Some(ResolvedCatalogParent {
                                backend: "show".into(),
                                requested: "show".into(),
                                member: member_parent_ids[&server.id].clone().into(),
                            })
                        } else {
                            None
                        },
                    })
                    .collect()
            };
            let result = match kind {
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
                _ => get_catalog(&state, req).await,
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
                parent: None,
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
            assert!(matches!(catalog, Err(ApplicationError::SourcesFailed(_))));
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
    let result = get_catalog(&state, req).await.unwrap();
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
    let result = get_catalog(&state, req).await.unwrap();
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
    let result = get_catalog(
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
        let result = get_catalog(
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
                parent: None,
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
    let series = get_virtual_library_items(
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
    let seasons = get_virtual_library_items(
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
                parent: None,
            })
            .collect();
        let response = get_virtual_library_items(
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
            parent: None,
        })
        .collect();
    let response = get_virtual_library_items(
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
    let member_parent_ids = fixture_member_parent_ids(&state, &sessions, "series").await;
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
            parent: Some(ResolvedCatalogParent {
                backend: "series".into(),
                member: member_parent_ids[&server.id].clone().into(),
                requested: aggregate.into(),
            }),
        })
        .collect();
    let response = get_aggregate_show_items(
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
    let member_parent_ids = fixture_member_parent_ids(&state, &sessions, "series").await;
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
                parent: Some(ResolvedCatalogParent {
                    backend: "series".into(),
                    member: member_parent_ids[&server.id].clone().into(),
                    requested: aggregate.into(),
                }),
            })
            .collect()
    };
    let seasons = get_aggregate_show_items(&state,
        request(&format!("/Shows/{aggregate}/Seasons?userId=viewer&Fields=ItemCounts,PrimaryImageAspectRatio,CanDelete,MediaSourceCount"), &sessions),
        aggregate.into(), targets(), 0).await.unwrap();
    let seasons = seasons["Items"].as_array().unwrap();
    assert_eq!(seasons.len(), 2);
    for (index, season) in seasons.iter().enumerate() {
        assert_eq!(season["SeriesId"], aggregate);
        assert_eq!(season["IndexNumber"], index + 1);
        let episodes = get_aggregate_show_items(
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
    crate::application::details::sources::merge_media_detail(
        &state,
        crate::application::details::sources::DetailMergeContext {
            requested_item_id: "selected-episode",
            selected_group: None,
            base_server: &sessions[0].1,
            auth: &None,
            access_scope: preprocessed.context.access_scope(),
            sessions: Some(&sessions),
            original_request: &preprocessed.original_request,
            source_generation: state
                .media_storage
                .begin_media_reconciliation()
                .await
                .unwrap(),
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
        let response =
            get_items_from_all_servers_if_not_restricted(&state, request(path, &sessions))
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
    let response = get_catalog(
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
    let degraded = get_catalog(&state, request("/Items/Latest?Limit=1", &sessions))
        .await
        .unwrap();
    assert_eq!(degraded[0]["Id"], stable_ids[0]);
    for active_sessions in [&sessions[..1]] {
        let response = get_catalog(&state, request("/Items/Latest?Limit=1", active_sessions))
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
        let response = get_catalog(&state, preprocessed)
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
    let response = get_catalog(&state, request("/Items/Latest?Limit=10", &sessions))
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
        let response = get_catalog(&state, request("/Items/Latest?Limit=10", &sessions))
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
    for path in ["/Items", "/Items/Suggestions"] {
        assert!(matches!(
            resolve_catalog_plan(&state, &request(path, &sessions))
                .await
                .unwrap(),
            CatalogPlan::Interleaved(_)
        ));
    }
    assert!(matches!(
        resolve_catalog_plan(&state, &request("/UserItems/Resume", &sessions))
            .await
            .unwrap(),
        CatalogPlan::Virtual { .. }
    ));
    state.config.write().await.deduplicate_media = false;
    assert!(matches!(
        resolve_catalog_plan(&state, &request("/UserItems/Resume", &sessions))
            .await
            .unwrap(),
        CatalogPlan::Interleaved(_)
    ));
}
