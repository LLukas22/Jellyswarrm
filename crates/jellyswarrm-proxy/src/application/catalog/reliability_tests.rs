//! Federated handler regressions for incomplete results and backend failure handling.
use super::media_reconciliation::{get_aggregate_show_items, get_virtual_library_items};
use super::{
    tests::{fixture_member_parent_ids, request, setup},
    *,
};
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, ResponseTemplate,
};

fn offset(request: &wiremock::Request) -> usize {
    request
        .url
        .query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("StartIndex"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0)
}

async fn mount_catalog(upstreams: &[wiremock::MockServer], sort: &str) {
    for upstream in upstreams {
        let sort = sort.to_string();
        Mock::given(method("GET"))
            .and(path("/Items"))
            .respond_with(move |request: &wiremock::Request| {
                // Random client sorts must use a repeatable backend scan.
                if sort == "Random" {
                    assert!(request
                        .url
                        .query_pairs()
                        .any(|(key, value)| key == "SortBy" && value == "SortName"));
                }
                let start = offset(request);
                ResponseTemplate::new(200).set_body_json(json!({
                    "Items": (0..6).skip(start).take(2).map(|index| json!({
                        "Id": format!("movie-{index}"), "Type": "Movie", "Name": "Equal",
                        "ProviderIds": {"Tmdb": index.to_string()}
                    })).collect::<Vec<_>>(), "TotalRecordCount": 6, "StartIndex": start
                }))
            })
            .mount(upstream)
            .await;
    }
}

#[tokio::test]
async fn counted_scans_apply_timeout_to_each_page_instead_of_the_whole_catalog() {
    let (state, _, sessions, upstreams) = setup().await;
    let service = &state.services;
    state.config.write().await.timeout = 1;
    for upstream in &upstreams {
        Mock::given(method("GET")).respond_with(|request: &wiremock::Request| {
            let start = offset(request);
            ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(400))
                .set_body_json(json!({"Items": [{"Id": format!("item-{start}"), "Type": "Movie"}], "StartIndex": start, "TotalRecordCount": 3}))
        }).mount(upstream).await;
    }
    let response = get_catalog(service, request("/Items?Limit=1", &sessions))
        .await
        .unwrap();
    assert_eq!(response["TotalRecordCount"], 6);
    assert_eq!(response["Items"].as_array().unwrap().len(), 1);
    assert_eq!(upstreams[0].received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn partial_search_membership_does_not_veto_recursive_catalog_parent_identity() {
    let (state, pool, mut sessions, mut upstreams) = setup().await;
    let third = wiremock::MockServer::start().await;
    let row = sqlx::query("INSERT INTO servers (name, url, priority, created_at, updated_at) VALUES ('Third', ?, 100, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) RETURNING *")
        .bind(third.uri()).fetch_one(&pool).await.unwrap();
    let mut session = sessions[0].0.clone();
    session.id = 2;
    session.server_url = third.uri();
    sessions.push((
        session,
        crate::servers::storage::Server::from_row(row).unwrap(),
    ));
    upstreams.push(third);
    for (index, upstream) in upstreams.iter().enumerate() {
        Mock::given(path("/Items")).respond_with(move |request: &wiremock::Request| {
            let search = request.url.query_pairs().any(|(key, _)| key == "SearchTerm");
            let mut items = vec![json!({"Id": "series", "Type": "Series", "Name": format!("Localized {index}"), "ProviderIds": {"Tmdb": "42"}})];
            if search && index == 2 { items.clear(); }
            if !search {
                items.push(json!({"Id": "season", "Type": "Season", "SeriesId": "series", "ParentId": "series", "IndexNumber": 1}));
                items.push(json!({"Id": "episode", "Type": "Episode", "SeriesId": "series", "SeasonId": "season", "ParentId": "season", "ParentIndexNumber": 1, "IndexNumber": 1}));
            }
            ResponseTemplate::new(200).set_body_json(json!({"TotalRecordCount": items.len(), "StartIndex": 0, "Items": items}))
        }).mount(upstream).await;
    }
    let service = &state.services;
    let search = get_catalog(service, request("/Items?SearchTerm=localized", &sessions))
        .await
        .unwrap();
    assert_eq!(search["TotalRecordCount"], 1);
    let mut previous = None;
    for _ in 0..2 {
        state.catalog_snapshots.expire();
        let targets = sessions
            .iter()
            .map(|(session, server)| CatalogFetchTarget {
                session: session.clone(),
                server: server.clone(),
                parent: None,
            })
            .collect();
        let response = get_virtual_library_items(
            service,
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
            3,
            "series, numbered season and numbered episode all merge"
        );
        let series = items.iter().find(|item| item["Type"] == "Series").unwrap();
        let season = items.iter().find(|item| item["Type"] == "Season").unwrap();
        let episode = items.iter().find(|item| item["Type"] == "Episode").unwrap();
        assert_eq!(season["SeriesId"], series["Id"]);
        assert_eq!(season["ParentId"], series["Id"]);
        assert_eq!(episode["SeriesId"], series["Id"]);
        assert_eq!(episode["SeasonId"], season["Id"]);
        assert_eq!(episode["ParentId"], season["Id"]);
        assert_eq!(episode["MediaSourceCount"], 3);
        if let Some(previous) = previous {
            assert_eq!(response, previous);
        }
        previous = Some(response);
    }
}

#[tokio::test]
async fn snapshots_coalesce_builds_and_keep_random_and_tied_pages_stable() {
    for sort in ["SortName", "Random"] {
        let (state, _, sessions, upstreams) = setup().await;
        let service = &state.services;
        mount_catalog(&upstreams, sort).await;
        let query = format!("/Items?SearchTerm=equal&SortBy={sort}");
        let fetch = |start, limit| {
            get_catalog(
                service,
                request(
                    &format!("{query}&StartIndex={start}&Limit={limit}"),
                    &sessions,
                ),
            )
        };
        let (first, second, count) = tokio::join!(fetch(0, 2), fetch(2, 2), fetch(0, 0));
        let first = first.unwrap();
        let second = second.unwrap();
        let count = count.unwrap();
        assert_eq!(count["Items"], json!([]));
        assert_eq!(count["TotalRecordCount"], 6);
        assert_eq!(second["StartIndex"], 2);
        assert!(first["Items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|left| second["Items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|right| left["Id"] != right["Id"])));
        assert_eq!(fetch(0, 2).await.unwrap(), first);
        assert_eq!(fetch(2, 2).await.unwrap(), second);
        assert_eq!(fetch(99, 3).await.unwrap()["TotalRecordCount"], 6);
        for upstream in &upstreams {
            assert_eq!(upstream.received_requests().await.unwrap().len(), 3);
        }
        // Expiration rebuilds, while persisted aggregate IDs remain stable.
        state.catalog_snapshots.expire();
        let rebuilt = fetch(0, 6).await.unwrap();
        assert_eq!(rebuilt["Items"].as_array().unwrap().len(), 6);
        for upstream in &upstreams {
            assert_eq!(upstream.received_requests().await.unwrap().len(), 6);
        }
    }
}

#[tokio::test]
async fn snapshots_are_scoped_to_viewer_token_targets_and_display_configuration() {
    use crate::{
        libraries::service::VirtualLibraryAccessScope, protocol::caller::JellyfinAuthorization,
    };
    let (state, _, sessions, upstreams) = setup().await;
    let service = &state.services;
    mount_catalog(&upstreams, "SortName").await;
    let build_request = |viewer: &str,
                         token: &str,
                         selected: &[(
        crate::auth::service::AuthorizationSession,
        crate::servers::storage::Server,
    )]| {
        let mut req = request("/Items?SearchTerm=equal&Limit=1", selected);
        req.context = crate::application::request::context::RequestContext::new(
            &req.original_request,
            Some(JellyfinAuthorization::XEmbyToken(token.to_string())),
            None,
            Some(selected.to_vec()),
            Some(VirtualLibraryAccessScope::new(
                viewer,
                selected.iter().map(|(_, server)| server.id),
            )),
        );
        req
    };
    let fetch =
        |viewer: &str, token: &str| get_catalog(service, build_request(viewer, token, &sessions));
    let first = fetch("viewer", "token-a").await.unwrap();
    assert_eq!(fetch("viewer", "token-a").await.unwrap(), first);
    let _ = fetch("viewer", "token-b").await.unwrap();
    let other = fetch("other", "token-a").await.unwrap();
    assert_ne!(first["Items"][0]["Id"], other["Items"][0]["Id"]);
    state.config.write().await.include_server_name_in_media = false;
    let _ = fetch("viewer", "token-a").await.unwrap();
    let _ = get_catalog(service, build_request("viewer", "token-a", &sessions[..1]))
        .await
        .unwrap();
    assert_eq!(upstreams[0].received_requests().await.unwrap().len(), 15);
    assert_eq!(upstreams[1].received_requests().await.unwrap().len(), 12);
}

#[tokio::test]
async fn incomplete_sources_are_discarded_without_caching_degraded_results_or_pruning_sightings() {
    use super::{
        library_resolution::CatalogFetchTarget, media_reconciliation::get_virtual_library_items,
    };
    let (state, pool, sessions, upstreams) = setup().await;
    let service = &state.services;
    mount_catalog(&upstreams, "SortName").await;
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
    let fetch = || {
        get_virtual_library_items(
            service,
            request("/Items?Recursive=true&Limit=1", &sessions),
            "configured:library:viewer".into(),
            targets(),
            0,
        )
    };
    let original = fetch().await.unwrap();
    for failure in ["total", "offset", "repeat", "empty", "failed", "deadline"] {
        state.catalog_snapshots.expire();
        upstreams[0].reset().await;
        state.config.write().await.timeout = 1;
        Mock::given(method("GET")).respond_with(move |request: &wiremock::Request| {
            let start = offset(request);
            if start == 0 {
                return ResponseTemplate::new(200).set_body_json(json!({"Items": [{"Id": "movie-0", "Type": "Movie", "ProviderIds": {"Tmdb": "0"}}], "TotalRecordCount": 2, "StartIndex": 0}));
            }
            if failure == "failed" { return ResponseTemplate::new(503); }
            if failure == "deadline" { return ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(2)); }
            ResponseTemplate::new(200).set_body_json(json!({
                "Items": if failure == "empty" { vec![] } else { vec![json!({"Id": if failure == "repeat" { "movie-0" } else { "movie-1" }, "Type": "Movie"})] },
                "TotalRecordCount": if failure == "total" { 3 } else { 2 },
                "StartIndex": if failure == "offset" { 0 } else { 1 }
            }))
        }).mount(&upstreams[0]).await;
        for _ in 0..2 {
            let response = fetch().await.unwrap();
            assert_eq!(response["TotalRecordCount"], 6, "{failure}");
            assert_eq!(response["Items"].as_array().unwrap().len(), 1);
            assert_eq!(
                response["Items"][0]["MediaSourceCount"], 1,
                "only the healthy source is visible"
            );
        }
        assert_eq!(
            upstreams[0].received_requests().await.unwrap().len(),
            4,
            "failed refresh must be retried"
        );
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM movie_catalog_sightings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 12, "{failure}");
        assert_eq!(
            state
                .media_storage
                .get_media_version_members_by_virtual_id(
                    &original["Items"][0]["Id"].as_str().unwrap().into()
                )
                .await
                .unwrap()
                .len(),
            2
        );
    }
    upstreams[0].reset().await;
    upstreams[1].reset().await;
    for upstream in &upstreams {
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"Items": [], "TotalRecordCount": 0, "StartIndex": 0})),
            )
            .mount(upstream)
            .await;
    }
    assert_eq!(fetch().await.unwrap()["TotalRecordCount"], 0);
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM movie_catalog_sightings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    state.catalog_snapshots.expire();
    for upstream in &upstreams {
        upstream.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(upstream)
            .await;
    }
    assert!(matches!(
        fetch().await.unwrap_err(),
        ApplicationError::Shared(cause) if matches!(cause.as_ref(), ApplicationError::SourcesFailed(_))
    ));
}

#[test]
fn sorting_uses_original_names_then_stable_ids_before_display_labels() {
    use super::postprocessing::{FederatedItems, ResponseShape};
    use crate::{media::presentation::ItemNamePolicy, protocol::models::MediaItem};
    let item = |id: &str| {
        serde_json::from_value::<MediaItem>(json!({"Id": id, "Name": "Equal", "Type": "Movie"}))
            .unwrap()
    };
    let items = vec![
        ItemNamePolicy::IncludeServerName.annotate(item("b"), "Alpha"),
        ItemNamePolicy::IncludeServerName.annotate(item("a"), "Zulu"),
    ];
    let response = FederatedItems::from_merged_items(items).into_response(
        &url::Url::parse("http://localhost/Items?SortBy=SortName").unwrap(),
        ResponseShape::Counted,
    );
    let value = serde_json::to_value(response).unwrap();
    assert_eq!(value["Items"][0]["Id"], "a");
    assert_eq!(value["Items"][0]["Name"], "Equal [Zulu]");
}

#[tokio::test]
async fn numbered_episode_versions_keep_ranges_distinct_and_route_same_server_copies() {
    use crate::{
        application::{
            details::sources::{merge_media_detail, DetailMergeContext},
            playback::{
                lifecycle::record_playback_sources,
                target::{MediaTargetResolver, PlaybackIntent, SelectionPolicy, SelectionReason},
            },
        },
        protocol::models::MediaSource,
        servers::storage::Server,
    };
    let (state, pool, mut sessions, mut upstreams) = setup().await;
    let third = wiremock::MockServer::start().await;
    let row = sqlx::query("INSERT INTO servers (name, url, priority, created_at, updated_at) VALUES ('Third', ?, 100, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) RETURNING *")
        .bind(third.uri()).fetch_one(&pool).await.unwrap();
    let mut session = sessions[0].0.clone();
    session.id = 2;
    session.server_url = third.uri();
    sessions.push((session, Server::from_row(row).unwrap()));
    upstreams.push(third);
    let owner = |number| format!("{number:032x}");
    let aggregate = "00000000000000000000000000000100";
    for (index, upstream) in upstreams.iter().enumerate() {
        Mock::given(method("GET"))
            .and(path("/System/Info/Public"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ServerName": "Backend"})),
            )
            .mount(upstream)
            .await;
        let mut episodes = vec![
            json!({"Id": owner(100), "Type": "Episode", "Name": "Pilot", "SeriesId": "series", "ParentIndexNumber": 1, "IndexNumber": 1, "ProviderIds": {"Tvdb": "42"}}),
            json!({"Id": owner(200), "Type": "Episode", "Name": "Combined", "SeriesId": "series", "ParentIndexNumber": 1, "IndexNumber": 1, "IndexNumberEnd": 2, "ProviderIds": {"Tvdb": "42"}}),
            json!({"Id": owner(300), "Type": "Episode", "Name": "Invalid range", "SeriesId": "series", "ParentIndexNumber": 1, "IndexNumber": 1, "IndexNumberEnd": 0, "ProviderIds": {"Tvdb": "42"}}),
        ];
        if index == 0 {
            let mut copy = episodes[0].clone();
            copy["Id"] = json!(owner(101));
            episodes.push(copy);
        }
        Mock::given(method("GET"))
            .and(path("/Shows/series/Episodes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"TotalRecordCount": episodes.len(), "StartIndex": 0, "Items": episodes}),
            ))
            .mount(upstream)
            .await;
        for number in if index == 0 {
            vec![100, 101]
        } else {
            vec![100]
        } {
            Mock::given(method("GET")).and(path(format!("/Items/{}", owner(number))))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Id": owner(number), "Type": "Episode", "Name": "Pilot", "MediaSources": [{"Id": owner(number + 1000), "Type": "Default"}]}))).mount(upstream).await;
        }
    }
    state.server_storage.check_servers_health().await;
    let member_parent_ids = fixture_member_parent_ids(&state, &sessions, "series").await;
    let targets = sessions
        .iter()
        .map(|(session, server)| CatalogFetchTarget {
            session: session.clone(),
            server: server.clone(),
            parent: Some(
                crate::application::catalog::library_resolution::ResolvedCatalogParent {
                    backend: "series".into(),
                    member: member_parent_ids[&server.id].clone().into(),
                    requested: aggregate.into(),
                },
            ),
        })
        .collect();
    let response = get_aggregate_show_items(
        &state,
        request(&format!("/Shows/{aggregate}/Episodes"), &sessions),
        aggregate.into(),
        targets,
        0,
    )
    .await
    .unwrap();
    let items = response["Items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        5,
        "single, combined, and three unmergeable invalid ranges"
    );
    let single = items
        .iter()
        .find(|item| item["MediaSourceCount"] == 4)
        .unwrap();
    let combined = items
        .iter()
        .find(|item| item["MediaSourceCount"] == 3)
        .unwrap();
    assert_eq!(combined["IndexNumberEnd"], 2);
    let single_id = single["Id"].as_str().unwrap();
    let group = state
        .media_storage
        .get_media_version_group(&single_id.into())
        .await
        .unwrap()
        .unwrap();
    let members = state
        .media_storage
        .get_media_version_members(group.id)
        .await
        .unwrap();
    assert_eq!(members.len(), 4);
    let base = members
        .iter()
        .find(|member| {
            member.mapping.server_id == sessions[0].1.id
                && member.mapping.original_media_id == owner(100).into()
        })
        .unwrap();
    let mut detail_request = request(&format!("/Items/{single_id}"), &sessions);
    let mut payload = json!({"Id": base.mapping.virtual_media_id.as_str(), "Type": "Episode", "Name": "Pilot", "MediaSources": []});
    // Use translated source IDs for the base item, as the real detail handler does.
    let source = state
        .media_storage
        .get_or_create_media_mapping(&owner(1100).as_str().into(), &sessions[0].1)
        .await
        .unwrap();
    payload["MediaSources"] = json!([{"Id": source.virtual_media_id.as_str(), "Type": "Default"}]);
    let generation = state
        .media_storage
        .begin_media_reconciliation()
        .await
        .unwrap();
    merge_media_detail(
        &state,
        DetailMergeContext {
            requested_item_id: single_id,
            selected_group: None,
            base_server: &sessions[0].1,
            auth: &None,
            access_scope: detail_request.context.access_scope(),
            sessions: Some(&sessions),
            original_request: &detail_request.original_request,
            source_generation: generation,
        },
        None,
        &mut payload,
    )
    .await
    .unwrap();
    assert_eq!(payload["MediaSourceCount"], 4);
    let sibling = members
        .iter()
        .find(|member| member.mapping.original_media_id == owner(101).into())
        .unwrap();
    let selected = state
        .media_storage
        .get_media_mapping_by_original(&owner(1101).as_str().into(), sessions[0].1.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        state
            .media_storage
            .get_media_version_source_route(group.id, &selected.virtual_media_id)
            .await
            .unwrap()
            .unwrap()
            .member_mapping
            .id,
        sibling.mapping.id
    );
    detail_request
        .original_request
        .url_mut()
        .set_path(&format!("/Items/{single_id}/PlaybackInfo"));
    let route = MediaTargetResolver {
        media: &state.media_storage,
    }
    .plan(
        &detail_request.context,
        detail_request.context.target(sessions[0].1.id).unwrap(),
        PlaybackIntent {
            item: &single_id.into(),
            source: Some(&selected.virtual_media_id),
            binding: None,
        },
        &state.server_storage,
        SelectionPolicy::Negotiation,
    )
    .await
    .unwrap();
    assert!(matches!(route.reason, SelectionReason::ExplicitSource));
    assert_eq!(route.target.backend.server().id, sessions[0].1.id);
    let member_virtual_id = &route.target.member.as_ref().unwrap().virtual_media_id;
    assert_eq!(member_virtual_id, &sibling.mapping.virtual_media_id);
    let mut normalized = detail_request.original_request.try_clone().unwrap();
    *normalized.url_mut() = crate::protocol::url::replace_path_id(
        normalized.url(),
        "Items",
        member_virtual_id.as_str(),
    )
    .unwrap();
    let built = state
        .request_builder()
        .build(&route.target.backend, &normalized)
        .await
        .unwrap();
    assert_eq!(
        built.url().path(),
        format!("/Items/{}/PlaybackInfo", owner(101))
    );
    let mut sources: Vec<MediaSource> = serde_json::from_value(
        json!([{"Id": selected.virtual_media_id.as_str(), "Type": "Default"}]),
    )
    .unwrap();
    let generation = state
        .media_storage
        .begin_media_reconciliation()
        .await
        .unwrap();
    record_playback_sources(
        &state.media_storage,
        &single_id.into(),
        route.target.backend.server(),
        &owner(101).as_str().into(),
        generation,
        &mut sources,
    )
    .await
    .unwrap();
    assert_eq!(
        state
            .media_storage
            .get_media_version_source_route(group.id, &selected.virtual_media_id)
            .await
            .unwrap()
            .unwrap()
            .member_mapping
            .id,
        sibling.mapping.id
    );
}

/// A backend whose listing is far larger than any home-screen row.
async fn mount_large_listing(upstream: &wiremock::MockServer, server: usize) {
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(r"/Items$"))
        .respond_with(move |request: &wiremock::Request| {
            let start = offset(request);
            let limit = request
                .url
                .query_pairs()
                .find(|(key, _)| key.eq_ignore_ascii_case("Limit"))
                .and_then(|(_, value)| value.parse().ok())
                .unwrap_or(100);
            ResponseTemplate::new(200).set_body_json(json!({
                "Items": (start..start + limit).map(|index| json!({
                    "Id": format!("server-{server}-item-{index}"), "Type": "Movie",
                    "Name": format!("Movie {index:05}"),
                    "ProviderIds": {"Tmdb": format!("{server}-{index}")}
                })).collect::<Vec<_>>(),
                "TotalRecordCount": 0, "StartIndex": start
            }))
        })
        .mount(upstream)
        .await;
}

fn query_value(request: &wiremock::Request, name: &str) -> Option<String> {
    request
        .url
        .query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.into_owned())
}

#[tokio::test]
async fn home_screen_rows_fetch_one_bounded_page_per_backend() {
    let (state, _, sessions, upstreams) = setup().await;
    for (index, upstream) in upstreams.iter().enumerate() {
        mount_large_listing(upstream, index).await;
    }
    // The favorites row from the webOS home screen (issue #231).
    let response = get_catalog(
        &state.services,
        request(
            "/Users/viewer/Items?SortBy=SeriesSortName,SortName&SortOrder=Ascending&Filters=IsFavorite&Recursive=true&EnableTotalRecordCount=false&Limit=20&IncludeItemTypes=Movie",
            &sessions,
        ),
    )
    .await
    .unwrap();
    assert_eq!(response["Items"].as_array().unwrap().len(), 20);
    for upstream in &upstreams {
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "a row must not scan the backend");
        assert_eq!(query_value(&requests[0], "Limit").as_deref(), Some("20"));
        assert_eq!(query_value(&requests[0], "StartIndex"), None);
        assert_eq!(
            query_value(&requests[0], "EnableTotalRecordCount").as_deref(),
            Some("false"),
            "keep the client's opt-out of the expensive upstream count"
        );
        assert_eq!(
            query_value(&requests[0], "Filters").as_deref(),
            Some("IsFavorite")
        );
    }
}

#[tokio::test]
async fn random_rows_sample_each_backend_once() {
    let (state, _, sessions, upstreams) = setup().await;
    for (index, upstream) in upstreams.iter().enumerate() {
        mount_large_listing(upstream, index).await;
    }
    // The search suggestions row from jellyfin-web.
    let response = get_catalog(
        &state.services,
        request(
            "/Items?userId=viewer&limit=20&recursive=true&includeItemTypes=Movie&includeItemTypes=Series&sortBy=IsFavoriteOrLiked&sortBy=Random&imageTypeLimit=0&enableTotalRecordCount=false&enableImages=false",
            &sessions,
        ),
    )
    .await
    .unwrap();
    assert_eq!(response["Items"].as_array().unwrap().len(), 20);
    for upstream in &upstreams {
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0]
            .url
            .query_pairs()
            .any(|(key, value)| key.eq_ignore_ascii_case("SortBy") && value == "Random"));
    }
}

#[tokio::test]
async fn rows_serve_available_backends_when_one_fails() {
    let (state, _, sessions, upstreams) = setup().await;
    mount_large_listing(&upstreams[0], 0).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&upstreams[1])
        .await;
    let response = get_catalog(
        &state.services,
        request(
            "/Items?Recursive=true&EnableTotalRecordCount=false&Limit=12",
            &sessions,
        ),
    )
    .await
    .unwrap();
    let items = response["Items"].as_array().unwrap();
    assert_eq!(items.len(), 12);
    assert!(items
        .iter()
        .all(|item| item["Name"].as_str().unwrap().starts_with("Movie")));
}

#[tokio::test]
async fn rows_fail_only_when_every_backend_fails() {
    let (state, _, sessions, upstreams) = setup().await;
    for upstream in &upstreams {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(upstream)
            .await;
    }
    assert!(get_catalog(
        &state.services,
        request(
            "/Items?Recursive=true&EnableTotalRecordCount=false&Limit=12",
            &sessions,
        ),
    )
    .await
    .is_err());
}

async fn mount_movies(upstream: &wiremock::MockServer, movies: Vec<(String, String)>) {
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(r"/Items$"))
        .respond_with(move |request: &wiremock::Request| {
            let start = offset(request);
            let limit = query_value(request, "Limit")
                .map(|value| value.parse().unwrap())
                .unwrap_or(movies.len());
            let counted =
                query_value(request, "EnableTotalRecordCount").as_deref() != Some("false");
            ResponseTemplate::new(200).set_body_json(json!({
                "Items": movies.iter().skip(start).take(limit).map(|(id, tmdb)| json!({
                    "Id": id, "Type": "Movie", "Name": format!("Movie {tmdb}"),
                    "ProviderIds": {"Tmdb": tmdb}
                })).collect::<Vec<_>>(),
                "TotalRecordCount": if counted { movies.len() } else { 0 }, "StartIndex": start
            }))
        })
        .mount(upstream)
        .await;
}

const FAVORITES_ROW: &str = "/Users/viewer/Items?SortBy=SeriesSortName,SortName&SortOrder=Ascending&Filters=IsFavorite&Recursive=true&EnableTotalRecordCount=false&Limit=20&IncludeItemTypes=Movie";

#[tokio::test]
async fn rows_without_a_library_collapse_copies_and_stay_full() {
    let (state, _, sessions, upstreams) = setup().await;
    for (server, upstream) in upstreams.iter().enumerate() {
        mount_movies(
            upstream,
            (0..20)
                .map(|index| (format!("server-{server}-{index}"), format!("{index}")))
                .collect(),
        )
        .await;
    }
    let response = get_catalog(&state.services, request(FAVORITES_ROW, &sessions))
        .await
        .unwrap();
    let items = response["Items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        20,
        "every movie exists twice, the row is still full"
    );
    let names = items
        .iter()
        .map(|item| item["Name"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names.len(), 20, "copies collapse into one item");
    assert!(items.iter().all(|item| item["MediaSourceCount"] == 2));
}

#[tokio::test]
async fn favorites_pages_use_the_same_merged_listing() {
    let (state, _, sessions, upstreams) = setup().await;
    for (server, upstream) in upstreams.iter().enumerate() {
        mount_movies(
            upstream,
            ["A", "B"]
                .map(|id| (format!("{server}-{id}"), id.to_owned()))
                .to_vec(),
        )
        .await;
    }
    let row = "/Items?SortBy=SortName&Filters=IsFavorite&EnableTotalRecordCount=false&Limit=1";
    for (start, name) in [(0, "Movie A"), (1, "Movie B")] {
        let page = get_catalog(
            &state.services,
            request(&format!("{row}&StartIndex={start}"), &sessions),
        )
        .await
        .unwrap();
        let items = page["Items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["Name"], name);
        assert_eq!(items[0]["MediaSourceCount"], 2);
    }
    let end = get_catalog(
        &state.services,
        request(&format!("{row}&StartIndex=2"), &sessions),
    )
    .await
    .unwrap();
    assert!(end["Items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn rows_with_backend_only_sorts_take_items_from_every_backend() {
    let (state, _, sessions, upstreams) = setup().await;
    for (server, upstream) in upstreams.iter().enumerate() {
        mount_movies(
            upstream,
            (0..20)
                .map(|index| {
                    (
                        format!("server-{server}-{index}"),
                        format!("{server}-{index}"),
                    )
                })
                .collect(),
        )
        .await;
    }
    // SeriesSortName is ranked by the backends, not the proxy.
    let response = get_catalog(&state.services, request(FAVORITES_ROW, &sessions))
        .await
        .unwrap();
    let items = response["Items"].as_array().unwrap();
    assert_eq!(items.len(), 20);
    for server in 0..2 {
        assert_eq!(
            items
                .iter()
                .filter(|item| item["Name"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("Movie {server}-")))
                .count(),
            10,
            "server {server}"
        );
    }
}

use std::sync::{Arc, Mutex};

/// A counted listing of `total` movies that returns at most `cap` per page.
async fn mount_counted_listing(
    upstream: &wiremock::MockServer,
    server: usize,
    total: usize,
    cap: usize,
    delay: std::time::Duration,
    arrivals: Arc<Mutex<Vec<(usize, std::time::Instant)>>>,
) {
    Mock::given(method("GET"))
        .and(path("/Items"))
        .respond_with(move |request: &wiremock::Request| {
            let start = offset(request);
            arrivals
                .lock()
                .unwrap()
                .push((start, std::time::Instant::now()));
            let limit = query_value(request, "Limit")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(total);
            let end = total.min(start + limit.min(cap));
            ResponseTemplate::new(200)
                .set_delay(delay)
                .set_body_json(json!({
                    "Items": (start..end).map(|index| json!({
                        "Id": format!("server-{server}-item-{index}"), "Type": "Movie",
                        "Name": format!("Movie {index:05}"),
                    })).collect::<Vec<_>>(),
                    "TotalRecordCount": total, "StartIndex": start
                }))
        })
        .mount(upstream)
        .await;
}

fn upstream_offsets(requests: &[wiremock::Request]) -> Vec<usize> {
    let mut offsets = requests.iter().map(offset).collect::<Vec<_>>();
    offsets.sort_unstable();
    offsets
}

#[tokio::test]
async fn full_scans_fetch_large_pages_several_at_a_time() {
    let (state, _, sessions, upstreams) = setup().await;
    let delay = std::time::Duration::from_secs(1);
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    for (server, upstream) in upstreams.iter().enumerate() {
        mount_counted_listing(upstream, server, 2_300, usize::MAX, delay, arrivals.clone()).await;
    }
    let response = get_catalog(
        &state.services,
        request("/Items?Recursive=true&Limit=10", &sessions),
    )
    .await
    .unwrap();
    assert_eq!(response["TotalRecordCount"], 4_600);
    // One page to learn the total, then the remaining four together: they
    // all arrive before the first of them could have been answered.
    let continuations = arrivals
        .lock()
        .unwrap()
        .iter()
        .filter(|(start, _)| *start > 0)
        .map(|(_, at)| *at)
        .collect::<Vec<_>>();
    let first = continuations.iter().min().unwrap();
    let last = continuations.iter().max().unwrap();
    assert!(
        last.duration_since(*first) < delay,
        "continuation pages were fetched one after another"
    );
    for upstream in &upstreams {
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(
            upstream_offsets(&requests),
            vec![0, 500, 1_000, 1_500, 2_000]
        );
        assert!(requests
            .iter()
            .all(|request| query_value(request, "Limit").as_deref() == Some("500")));
    }
}

#[tokio::test]
async fn compact_scan_releases_staged_records_after_a_failed_continuation() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (state, _, sessions, upstreams) = setup().await;
    let saw_staged_items = Arc::new(AtomicBool::new(false));
    let snapshots = Arc::clone(&state.catalog_snapshots);
    let saw_staged = Arc::clone(&saw_staged_items);
    Mock::given(method("GET"))
        .and(path("/Items"))
        .respond_with(move |request: &wiremock::Request| {
            if offset(request) == 0 {
                ResponseTemplate::new(200).set_body_json(json!({
                    "Items": [{"Id": "staged-only", "Type": "Movie"}],
                    "StartIndex": 0,
                    "TotalRecordCount": 2
                }))
            } else {
                saw_staged.store(snapshots.store_stats().records > 0, Ordering::Relaxed);
                ResponseTemplate::new(503)
            }
        })
        .mount(&upstreams[0])
        .await;
    Mock::given(method("GET"))
        .and(path("/Items"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Items": [],
            "StartIndex": 0,
            "TotalRecordCount": 0
        })))
        .mount(&upstreams[1])
        .await;

    let response = get_catalog(
        &state.services,
        request("/Items?Recursive=true&Limit=10", &sessions),
    )
    .await
    .unwrap();
    assert_eq!(response["Items"], json!([]));
    assert_eq!(response["TotalRecordCount"], 0);
    assert!(saw_staged_items.load(Ordering::Relaxed));
    assert_eq!(state.catalog_snapshots.store_stats().records, 0);
}

#[tokio::test]
async fn full_scans_resume_after_a_short_page_without_gaps_or_duplicates() {
    let (state, _, sessions, upstreams) = setup().await;
    for (server, upstream) in upstreams.iter().enumerate() {
        // The backend caps every page at 300 items, below the requested 500.
        mount_counted_listing(
            upstream,
            server,
            1_000,
            300,
            Default::default(),
            Default::default(),
        )
        .await;
    }
    let response = get_catalog(
        &state.services,
        request("/Items?Recursive=true&Limit=2000", &sessions),
    )
    .await
    .unwrap();
    let items = response["Items"].as_array().unwrap();
    assert_eq!(response["TotalRecordCount"], 2_000);
    assert_eq!(
        items
            .iter()
            .map(|item| item["Id"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        2_000
    );
    for upstream in &upstreams {
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(upstream_offsets(&requests), vec![0, 300, 600, 900]);
    }
}

/// Run explicitly when changing scan or snapshot performance.
#[tokio::test]
#[ignore = "scans two 22,000-episode catalogs"]
async fn large_episode_catalog_reuses_snapshot_for_deep_pages() {
    let (state, _, sessions, upstreams) = setup().await;
    for (server, upstream) in upstreams.iter().enumerate() {
        Mock::given(method("GET"))
            .and(path("/Items"))
            .respond_with(move |req: &wiremock::Request| {
                let start = offset(req);
                let limit: usize = query_value(req, "Limit").unwrap().parse().unwrap();
                let items = (start..22_000.min(start + limit))
                    .map(|index| {
                        json!({
                            "Id": format!("{:032x}", 1 + server * 22_000 + index),
                            "Type": "Episode",
                            "ProviderIds": {"Tvdb": (index + 1).to_string()},
                            "Name": format!("Episode {index:05}"),
                            "Overview": "description ".repeat(40),
                            "IndexNumber": index + 1,
                            "ParentIndexNumber": 1
                        })
                    })
                    .collect::<Vec<_>>();
                ResponseTemplate::new(200).set_body_json(json!({
                    "Items": items, "TotalRecordCount": 22_000, "StartIndex": start
                }))
            })
            .mount(upstream)
            .await;
    }
    for merged in [false, true] {
        let total = if merged { 22_000 } else { 44_000 };
        let copies = if merged { 1 } else { 2 };
        for start in [0, total - 10, total / 2, total] {
            let url = format!("/Items?Recursive=true&IncludeItemTypes=Episode&SortBy=SortName&Limit=10&StartIndex={start}");
            let request = request(&url, &sessions);
            let began = std::time::Instant::now();
            let page = if merged {
                let targets = sessions
                    .iter()
                    .map(|(session, server)| CatalogFetchTarget {
                        session: session.clone(),
                        server: server.clone(),
                        parent: None,
                    })
                    .collect();
                get_virtual_library_items(
                    &state.services,
                    request,
                    "configured:episodes:viewer".into(),
                    targets,
                    0,
                )
                .await
            } else {
                get_catalog(&state.services, request).await
            }
            .unwrap();
            eprintln!(
                "merged={merged}, offset={start}, elapsed={:?}",
                began.elapsed()
            );
            assert_eq!(page["TotalRecordCount"], total);
            assert_eq!(page["StartIndex"], start);
            let items = page["Items"].as_array().unwrap();
            assert_eq!(items.len(), if start == total { 0 } else { 10 });
            for (i, item) in items.iter().enumerate() {
                assert_eq!(item["IndexNumber"], (start + i) / copies + 1);
            }
            for upstream in &upstreams {
                let scans = if merged { 2 } else { 1 };
                let expected = (0..22_000)
                    .step_by(500)
                    .flat_map(|n| std::iter::repeat_n(n, scans))
                    .collect::<Vec<_>>();
                assert_eq!(
                    upstream_offsets(&upstream.received_requests().await.unwrap()),
                    expected
                );
            }
        }
    }
}

/// Two movies per backend whose watched state follows `played`. Batch `Ids`
/// lookups return only the requested movies.
async fn mount_watchable(
    upstream: &wiremock::MockServer,
    server: usize,
    played: Arc<std::sync::atomic::AtomicBool>,
) {
    mount_watchable_movies(upstream, server, 2, None, played).await;
}

/// `count` movies on one backend. With `shared`, every backend's movie N has
/// the same TMDB ID, so the copies are merged.
async fn mount_watchable_movies(
    upstream: &wiremock::MockServer,
    server: usize,
    count: usize,
    shared: Option<()>,
    played: Arc<std::sync::atomic::AtomicBool>,
) {
    Mock::given(method("GET"))
        .and(path("/Items"))
        .respond_with(move |request: &wiremock::Request| {
            let ids = query_value(request, "Ids");
            let played = played.load(std::sync::atomic::Ordering::SeqCst);
            let items = (0..count)
                .map(|index| (index, format!("server-{server}-movie-{index}")))
                .filter(|(_, id)| ids.as_ref().is_none_or(|ids| ids.split(',').any(|wanted| wanted == id)))
                .map(|(index, id)| json!({
                    "Id": id, "Type": "Movie",
                    "Name": shared.map_or_else(|| id.clone(), |()| format!("Movie {index}")),
                    "ProviderIds": if shared.is_some() { json!({"Tmdb": index.to_string()}) } else { json!({}) },
                    "UserData": {"Played": played, "Key": id, "ItemId": id, "PlaybackPositionTicks": 0, "PlayCount": 0, "IsFavorite": false}
                }))
                .collect::<Vec<_>>();
            ResponseTemplate::new(200).set_body_json(json!({
                "TotalRecordCount": items.len(), "StartIndex": offset(request), "Items": items
            }))
        })
        .mount(upstream)
        .await;
}

async fn listing_scans(upstreams: &[wiremock::MockServer]) -> usize {
    let mut scans = 0;
    for upstream in upstreams {
        scans += upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| query_value(request, "Ids").is_none())
            .count();
    }
    scans
}

#[tokio::test]
async fn watched_changes_refresh_the_page_instead_of_rescanning_the_library() {
    let (state, _, sessions, upstreams) = setup().await;
    let played = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for (server, upstream) in upstreams.iter().enumerate() {
        mount_watchable(upstream, server, played.clone()).await;
    }
    let listing = || {
        get_catalog(
            &state.services,
            request("/Items?Recursive=true&Limit=10", &sessions),
        )
    };
    let watched = |response: &serde_json::Value| {
        response["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["UserData"]["Played"].as_bool().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(watched(&listing().await.unwrap()), vec![false; 4]);
    let scans = listing_scans(&upstreams).await;

    played.store(true, std::sync::atomic::Ordering::SeqCst);
    state.catalog_snapshots.user_data_changed("viewer");
    assert_eq!(watched(&listing().await.unwrap()), vec![true; 4]);
    assert_eq!(
        listing_scans(&upstreams).await,
        scans,
        "inventory is reused"
    );

    // Any other write discards the cached listing.
    state.catalog_snapshots.invalidate_viewer("viewer");
    listing().await.unwrap();
    assert!(listing_scans(&upstreams).await > scans);
}

#[tokio::test]
async fn listings_filtered_by_watched_state_are_rebuilt_after_watched_changes() {
    let (state, _, sessions, upstreams) = setup().await;
    let played = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for (server, upstream) in upstreams.iter().enumerate() {
        mount_watchable(upstream, server, played.clone()).await;
    }
    let unwatched = || {
        get_catalog(
            &state.services,
            request(
                "/Items?Recursive=true&Filters=IsUnplayed&Limit=10",
                &sessions,
            ),
        )
    };
    unwatched().await.unwrap();
    let scans = listing_scans(&upstreams).await;
    unwatched().await.unwrap();
    assert_eq!(
        listing_scans(&upstreams).await,
        scans,
        "cached while unchanged"
    );
    state.catalog_snapshots.user_data_changed("viewer");
    unwatched().await.unwrap();
    assert!(
        listing_scans(&upstreams).await > scans,
        "membership may have changed"
    );
}

#[tokio::test]
async fn stale_listings_are_answered_at_once_and_rebuilt_in_the_background() {
    let (state, _, sessions, upstreams) = setup().await;
    let played = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for (server, upstream) in upstreams.iter().enumerate() {
        mount_watchable(upstream, server, played.clone()).await;
    }
    let listing = || {
        get_catalog(
            &state.services,
            request("/Items?Recursive=true&Limit=10", &sessions),
        )
    };
    let first = listing().await.unwrap();
    let scans = listing_scans(&upstreams).await;
    state.catalog_snapshots.age();
    assert_eq!(
        listing().await.unwrap(),
        first,
        "the stale listing is served"
    );
    state.catalog_snapshots.settle().await;
    assert!(
        listing_scans(&upstreams).await > scans,
        "and rebuilt behind it"
    );
    let rescanned = listing_scans(&upstreams).await;
    listing().await.unwrap();
    assert_eq!(
        listing_scans(&upstreams).await,
        rescanned,
        "the rebuild is fresh"
    );
}

fn watched(response: &serde_json::Value) -> Vec<bool> {
    response["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["UserData"]["Played"].as_bool().unwrap())
        .collect()
}

#[tokio::test]
async fn watched_changes_refresh_merged_copies() {
    let (state, _, sessions, upstreams) = setup().await;
    let played = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for (server, upstream) in upstreams.iter().enumerate() {
        mount_watchable_movies(upstream, server, 2, Some(()), played.clone()).await;
    }
    let listing = || {
        let targets = sessions
            .iter()
            .map(
                |(session, server)| super::library_resolution::CatalogFetchTarget {
                    session: session.clone(),
                    server: server.clone(),
                    parent: None,
                },
            )
            .collect();
        get_virtual_library_items(
            &state.services,
            request("/Items?Recursive=true&Limit=10", &sessions),
            "configured:movies:viewer".into(),
            targets,
            0,
        )
    };
    let first = listing().await.unwrap();
    assert_eq!(watched(&first), vec![false; 2], "copies are merged");
    let scans = listing_scans(&upstreams).await;

    played.store(true, std::sync::atomic::Ordering::SeqCst);
    state.catalog_snapshots.user_data_changed("viewer");
    assert_eq!(watched(&listing().await.unwrap()), vec![true; 2]);
    assert_eq!(
        listing_scans(&upstreams).await,
        scans,
        "inventory is reused"
    );
}

#[tokio::test]
async fn unlimited_listings_are_rebuilt_after_watched_changes() {
    let (state, _, sessions, upstreams) = setup().await;
    let played = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for (server, upstream) in upstreams.iter().enumerate() {
        mount_watchable(upstream, server, played.clone()).await;
    }
    let everything = || get_catalog(&state.services, request("/Items?Recursive=true", &sessions));
    everything().await.unwrap();
    let scans = listing_scans(&upstreams).await;
    played.store(true, std::sync::atomic::Ordering::SeqCst);
    state.catalog_snapshots.user_data_changed("viewer");
    assert_eq!(watched(&everything().await.unwrap()), vec![true; 4]);
    assert!(
        listing_scans(&upstreams).await > scans,
        "a whole library is not refreshed by ID"
    );
}

#[tokio::test]
async fn large_pages_refresh_user_data_in_bounded_lookups() {
    let (state, _, sessions, upstreams) = setup().await;
    let played = Arc::new(std::sync::atomic::AtomicBool::new(false));
    mount_watchable_movies(&upstreams[0], 0, 250, None, played.clone()).await;
    mount_watchable_movies(&upstreams[1], 1, 0, None, played.clone()).await;
    let listing = || {
        get_catalog(
            &state.services,
            request("/Items?Recursive=true&Limit=300", &sessions),
        )
    };
    listing().await.unwrap();
    played.store(true, std::sync::atomic::Ordering::SeqCst);
    state.catalog_snapshots.user_data_changed("viewer");
    assert_eq!(watched(&listing().await.unwrap()), vec![true; 250]);
    let lookups = upstreams[0]
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|request| query_value(&request, "Ids"))
        .map(|ids| ids.split(',').count())
        .collect::<Vec<_>>();
    assert_eq!(lookups.iter().sum::<usize>(), 250);
    assert!(lookups.iter().all(|&ids| ids <= 100), "{lookups:?}");
}
