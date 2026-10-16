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
    state.config.write().await.timeout = 1;
    for upstream in &upstreams {
        Mock::given(method("GET")).respond_with(|request: &wiremock::Request| {
            let start = offset(request);
            ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(400))
                .set_body_json(json!({"Items": [{"Id": format!("item-{start}"), "Type": "Movie"}], "StartIndex": start, "TotalRecordCount": 3}))
        }).mount(upstream).await;
    }
    let response =
        get_items_from_all_servers_preprocessed(&state, request("/Items?Limit=1", &sessions))
            .await
            .unwrap()
            .0;
    assert_eq!(response["TotalRecordCount"], 6);
    assert_eq!(response["Items"].as_array().unwrap().len(), 1);
    assert_eq!(upstreams[0].received_requests().await.unwrap().len(), 3);
}

/// This request used to fail while initializing an arbitrary single backend:
/// its first source-owned facet belongs to an authorized but offline source.
#[tokio::test]
async fn http_catalog_input_validates_offline_ids_without_initial_backend_selection() {
    use tower::ServiceExt;

    let (state, _, sessions, upstreams) = setup().await;
    let user = state
        .user_authorization
        .create_user("catalog-input", &"password".into())
        .await
        .unwrap();
    let authorization = crate::models::Authorization {
        client: "test".into(),
        device: "test".into(),
        device_id: "test".into(),
        version: "1".into(),
        token: None,
    };
    for (_, server) in &sessions {
        state
            .user_authorization
            .add_server_mapping(&user.id, server, "catalog-input", &"password".into(), None)
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
    }
    Mock::given(path("/System/Info/Public"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ServerName":"Online"})))
        .mount(&upstreams[1])
        .await;
    Mock::given(path("/Items"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Items":[{"Id":"movie","Type":"Movie"}], "TotalRecordCount":1,"StartIndex":0,
        })))
        .mount(&upstreams[1])
        .await;
    state.server_storage.check_servers_health().await;
    let mut facets = Vec::new();
    for (_, server) in &sessions {
        facets.push(
            state
                .media_storage
                .get_or_create_media_mapping("genre", server)
                .await
                .unwrap()
                .virtual_media_id,
        );
    }
    let app = axum::Router::new()
        .route(
            "/Items",
            axum::routing::get(get_items_from_all_servers_if_not_restricted),
        )
        .with_state(state.clone());
    let send = |uri: String| {
        let app = app.clone();
        let auth = format!("MediaBrowser Client=\"test\", Device=\"test\", DeviceId=\"test\", Version=\"1\", Token=\"{}\"", user.virtual_key);
        async move {
            app.oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .header("Authorization", auth)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    let response = send(format!("/Items?GenreIds={},{}", facets[0], facets[1])).await;
    assert_eq!(response.status(), StatusCode::OK);
    let online = upstreams[1].received_requests().await.unwrap();
    let catalog_requests = online
        .iter()
        .filter(|request| request.url.path() == "/Items")
        .collect::<Vec<_>>();
    assert_eq!(catalog_requests.len(), 1);
    assert!(catalog_requests[0]
        .url
        .query_pairs()
        .any(|(key, value)| key == "GenreIds" && value == "genre"));
    let before = online.len();

    // A concrete offline owner still has the legacy FORBIDDEN contract,
    // rather than being redirected to the healthy source.
    assert_eq!(
        send(format!("/Items?SeriesId={}", facets[0]))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        upstreams[1].received_requests().await.unwrap().len(),
        before
    );

    // Removing access to the offline source rejects its IDs before any fan-out.
    let mapping = state
        .user_authorization
        .get_server_mapping_by_server_id(&user.id, sessions[0].1.id)
        .await
        .unwrap()
        .unwrap();
    state
        .user_authorization
        .delete_server_mapping(mapping.id)
        .await
        .unwrap();
    assert_eq!(
        send(format!("/Items?GenreIds={},{}", facets[0], facets[1]))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        upstreams[1].received_requests().await.unwrap().len(),
        before
    );

    // All unavailable is still 503, including an otherwise federated query.
    upstreams[1].reset().await;
    state.server_storage.check_servers_health().await;
    let before = upstreams[1].received_requests().await.unwrap().len();
    for uri in [
        "/Items".to_string(),
        format!("/Items?SeriesId={}", facets[1]),
    ] {
        assert_eq!(send(uri).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
    assert_eq!(
        upstreams[1].received_requests().await.unwrap().len(),
        before
    );
    assert!(upstreams[0]
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| request.url.path() == "/System/Info/Public"));
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
        crate::server_storage::Server::from_row(row).unwrap(),
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
    let search = get_items_from_all_servers_preprocessed(
        &state,
        request("/Items?SearchTerm=localized", &sessions),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(search["TotalRecordCount"], 1);
    let mut previous = None;
    for _ in 0..2 {
        state.catalog_snapshots.expire();
        let targets = sessions
            .iter()
            .map(|(session, server)| CatalogFetchTarget {
                session: session.clone(),
                server: server.clone(),
                parent_id: None,
                member_parent_id: None,
                resolved_parent_id: None,
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
        .unwrap()
        .0;
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
        mount_catalog(&upstreams, sort).await;
        let query = format!("/Items?SearchTerm=equal&SortBy={sort}");
        let fetch = |start, limit| {
            get_items_from_all_servers_preprocessed(
                &state,
                request(
                    &format!("{query}&StartIndex={start}&Limit={limit}"),
                    &sessions,
                ),
            )
        };
        let (first, second, count) = tokio::join!(fetch(0, 2), fetch(2, 2), fetch(0, 0));
        let first = first.unwrap().0;
        let second = second.unwrap().0;
        let count = count.unwrap().0;
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
        assert_eq!(fetch(0, 2).await.unwrap().0, first);
        assert_eq!(fetch(2, 2).await.unwrap().0, second);
        assert_eq!(fetch(99, 3).await.unwrap().0["TotalRecordCount"], 6);
        for upstream in &upstreams {
            assert_eq!(upstream.received_requests().await.unwrap().len(), 3);
        }
        // Expiration rebuilds, while persisted aggregate IDs remain stable.
        state.catalog_snapshots.expire();
        let rebuilt = fetch(0, 6).await.unwrap().0;
        assert_eq!(rebuilt["Items"].as_array().unwrap().len(), 6);
        for upstream in &upstreams {
            assert_eq!(upstream.received_requests().await.unwrap().len(), 6);
        }
    }
}

#[tokio::test]
async fn snapshots_are_scoped_to_viewer_token_targets_and_display_configuration() {
    use crate::{
        request_preprocessing::JellyfinAuthorization,
        virtual_library_service::VirtualLibraryAccessScope,
    };
    let (state, _, sessions, upstreams) = setup().await;
    mount_catalog(&upstreams, "SortName").await;
    let build_request = |viewer: &str,
                         token: &str,
                         selected: &[(
        crate::user_authorization_service::AuthorizationSession,
        crate::server_storage::Server,
    )]| {
        let mut req = request("/Items?SearchTerm=equal&Limit=1", selected);
        req.auth = Some(JellyfinAuthorization::XEmbyToken(token.to_string()));
        req.access_scope = Some(VirtualLibraryAccessScope::new(
            viewer,
            selected.iter().map(|(_, server)| server.id),
        ));
        req
    };
    let fetch = |viewer: &str, token: &str| {
        get_items_from_all_servers_preprocessed(&state, build_request(viewer, token, &sessions))
    };
    let first = fetch("viewer", "token-a").await.unwrap().0;
    assert_eq!(fetch("viewer", "token-a").await.unwrap().0, first);
    let _ = fetch("viewer", "token-b").await.unwrap();
    let other = fetch("other", "token-a").await.unwrap().0;
    assert_ne!(first["Items"][0]["Id"], other["Items"][0]["Id"]);
    state.config.write().await.include_server_name_in_media = false;
    let _ = fetch("viewer", "token-a").await.unwrap();
    let _ = get_items_from_all_servers_preprocessed(
        &state,
        build_request("viewer", "token-a", &sessions[..1]),
    )
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
    mount_catalog(&upstreams, "SortName").await;
    let targets = || {
        sessions
            .iter()
            .map(|(session, server)| CatalogFetchTarget {
                session: session.clone(),
                server: server.clone(),
                parent_id: None,
                member_parent_id: None,
                resolved_parent_id: None,
            })
            .collect()
    };
    let fetch = || {
        get_virtual_library_items(
            &state,
            request("/Items?Recursive=true&Limit=1", &sessions),
            "configured:library:viewer".into(),
            targets(),
            0,
        )
    };
    let original = fetch().await.unwrap().0;
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
            let response = fetch().await.unwrap().0;
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
                    original["Items"][0]["Id"].as_str().unwrap()
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
    assert_eq!(fetch().await.unwrap().0["TotalRecordCount"], 0);
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
    assert_eq!(fetch().await.unwrap_err(), StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn sorting_uses_original_names_then_stable_ids_before_display_labels() {
    use super::postprocessing::{FederatedItems, ResponseShape};
    use crate::{media_presentation::ItemNamePolicy, models::MediaItem};
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
            playback::selection::{
                record_playback_sources, resolve_playback_route, PlaybackRouteDecision,
            },
        },
        models::MediaSource,
        server_storage::Server,
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
            parent_id: Some("series".into()),
            member_parent_id: Some(member_parent_ids[&server.id].clone()),
            resolved_parent_id: Some(aggregate.into()),
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
    .unwrap()
    .0;
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
        .get_media_version_group(single_id)
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
                && member.mapping.original_media_id == owner(100)
        })
        .unwrap();
    let mut detail_request = request(&format!("/Items/{single_id}"), &sessions);
    let mut payload = json!({"Id": base.mapping.virtual_media_id, "Type": "Episode", "Name": "Pilot", "MediaSources": []});
    // Use translated source IDs for the base item, as the real detail handler does.
    let source = state
        .media_storage
        .get_or_create_media_mapping(&owner(1100), &sessions[0].1)
        .await
        .unwrap();
    payload["MediaSources"] = json!([{"Id": source.virtual_media_id, "Type": "Default"}]);
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
            access_scope: detail_request.access_scope.as_ref(),
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
        .find(|member| member.mapping.original_media_id == owner(101))
        .unwrap();
    let selected = state
        .media_storage
        .get_media_mapping_by_original(&owner(1101), sessions[0].1.id)
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
    let context = crate::application::context::RequestContext::new(
        &detail_request.original_request,
        detail_request.auth.clone(),
        detail_request.user.clone(),
        detail_request.sessions.clone(),
        detail_request.access_scope.clone(),
    );
    let route = resolve_playback_route(
        &state,
        &context,
        single_id,
        Some(&selected.virtual_media_id),
    )
    .await
    .unwrap();
    let PlaybackRouteDecision::Rerouted(route) = route else {
        panic!("same-server copy must reroute its item ID");
    };
    assert_eq!(route.target.server().id, sessions[0].1.id);
    assert_eq!(route.member_virtual_id, sibling.mapping.virtual_media_id);
    let mut normalized = detail_request.original_request.try_clone().unwrap();
    *normalized.url_mut() =
        crate::url_helper::replace_path_id(normalized.url(), "Items", &route.member_virtual_id)
            .unwrap();
    let prefix = state.get_url_prefix().await;
    let built = crate::upstream::jellyfin::request::UpstreamRequestBuilder::new(
        &state.processors.url_processor,
        prefix.as_deref(),
    )
    .build(&route.target, &normalized)
    .await
    .unwrap();
    assert_eq!(
        built.url().path(),
        format!("/Items/{}/PlaybackInfo", owner(101))
    );
    let mut sources: Vec<MediaSource> =
        serde_json::from_value(json!([{"Id": selected.virtual_media_id, "Type": "Default"}]))
            .unwrap();
    let generation = state
        .media_storage
        .begin_media_reconciliation()
        .await
        .unwrap();
    record_playback_sources(
        &state,
        single_id,
        route.target.server(),
        &owner(101),
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
