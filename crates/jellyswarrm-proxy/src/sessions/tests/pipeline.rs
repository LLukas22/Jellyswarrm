use super::*;
use crate::{
    media_identity::{MediaAlias, MediaObservation},
    media_storage_service::{MediaCatalogSnapshot, MediaVersionSourceObservation},
};
use std::sync::atomic::{AtomicBool, Ordering};
use wiremock::{
    matchers::{method, path},
    Mock, ResponseTemplate,
};

async fn healthy(f: &Fixture) {
    for upstream in &f.upstreams {
        Mock::given(path("/System/Info/Public"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ServerName": "Backend"})),
            )
            .mount(upstream)
            .await;
    }
    f.state.server_storage.check_servers_health().await;
}

async fn catalog(f: &Fixture, path: &str) -> Value {
    let response = f
        .request(reqwest::Method::GET, path, &f.user, "tv")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    response.json().await.unwrap()
}

#[tokio::test]
async fn same_server_episode_selection_survives_streams_and_all_playback_reports() {
    let f = Fixture::new().await;
    healthy(&f).await;
    let server = f
        .state
        .server_storage
        .list_servers()
        .await
        .unwrap()
        .remove(0);
    let upstream = f
        .upstreams
        .iter()
        .find(|upstream| server.url.as_str().trim_end_matches('/') == upstream.uri())
        .unwrap();
    let mut members = Vec::new();
    let mut sources = Vec::new();
    for number in [100, 101] {
        members.push(
            f.state
                .media_storage
                .get_or_create_media_mapping(&format!("{number:032x}"), &server)
                .await
                .unwrap(),
        );
        sources.push(
            f.state
                .media_storage
                .get_or_create_media_mapping(&format!("{:032x}", number + 1000), &server)
                .await
                .unwrap(),
        );
    }
    let episode = serde_json::from_value(
        json!({"Id": "episode", "Type": "Episode", "ParentIndexNumber": 1, "IndexNumber": 1}),
    )
    .unwrap();
    let aliases =
        std::collections::BTreeSet::from([
            MediaAlias::for_episode(&episode, "matched-series").unwrap()
        ]);
    let generation = f
        .state
        .media_storage
        .begin_media_reconciliation()
        .await
        .unwrap();
    let groups = f
        .state
        .media_storage
        .reconcile_media_catalog(
            &format!("aggregate:series:{}", f.user.id),
            generation,
            &[MediaCatalogSnapshot {
                source_key: "episodes".into(),
                server_id: server.id,
                complete: true,
                observations: members
                    .iter()
                    .map(|member| MediaObservation {
                        virtual_media_id: member.virtual_media_id.clone(),
                        aliases: aliases.clone(),
                    })
                    .collect(),
            }],
            true,
        )
        .await
        .unwrap();
    let aggregate = &groups[&members[0].virtual_media_id].virtual_media_id;
    let group = f
        .state
        .media_storage
        .get_media_version_group(aggregate)
        .await
        .unwrap()
        .unwrap();
    f.state
        .media_storage
        .replace_media_version_sources(
            group.id,
            generation,
            &members.iter().map(|member| member.id).collect::<Vec<_>>(),
            &members
                .iter()
                .zip(&sources)
                .map(|(member, source)| MediaVersionSourceObservation {
                    member_mapping_id: member.id,
                    source_virtual_id: source.virtual_media_id.clone(),
                })
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();

    // Exercise both copies: neither SQL row order nor the canonical item wins.
    for (index, (member, source)) in members.iter().zip(&sources).enumerate() {
        let play_session = format!("playback-{index}");
        Mock::given(method("POST")).and(path(format!("/Items/{}/PlaybackInfo", member.original_media_id)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "PlaySessionId": play_session,
                "MediaSources": [{"Id": source.original_media_id, "Type": "Default", "SupportsDirectPlay": true}]
            }))).expect(1).mount(upstream).await;
        let response = f
            .request(
                reqwest::Method::POST,
                &format!("/Items/{aggregate}/PlaybackInfo"),
                &f.user,
                "tv",
            )
            .json(&json!({"MediaSourceId": source.virtual_media_id, "DeviceProfile": {}}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let playback: Value = response.json().await.unwrap();
        assert_eq!(playback["PlaySessionId"], play_session);
        let conflicting = f.request(reqwest::Method::POST, "/Sessions/Playing/Progress", &f.user, "tv")
            .json(&json!({"ItemId": aggregate, "PlaySessionId": play_session, "MediaSourceId": sources[1 - index].virtual_media_id}))
            .send().await.unwrap();
        assert_eq!(conflicting.status(), StatusCode::BAD_REQUEST);

        Mock::given(method("GET"))
            .and(path(format!(
                "/Videos/{}/stream.mp4",
                member.original_media_id
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string("video"))
            .mount(upstream)
            .await;
        // With explicit selection, and with only the session's exact binding.
        for query in [
            format!("MediaSourceId={}", source.virtual_media_id),
            format!("PlaySessionId={play_session}"),
        ] {
            let response = f
                .request(
                    reqwest::Method::GET,
                    &format!("/Videos/{aggregate}/stream.mp4?{query}"),
                    &f.user,
                    "tv",
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.text().await.unwrap(), "video");
        }
        for (endpoint, include_source) in [
            ("/Sessions/Playing", true),
            ("/Sessions/Playing/Progress", false),
            ("/Sessions/Playing/Stopped", false),
        ] {
            let expected_item = member.original_media_id.clone();
            let expected_source = source.original_media_id.clone();
            Mock::given(method("POST"))
                .and(path(endpoint))
                .respond_with(move |request: &wiremock::Request| {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    assert_eq!(body["ItemId"], expected_item);
                    if include_source {
                        assert_eq!(body["MediaSourceId"], expected_source);
                    }
                    ResponseTemplate::new(204)
                })
                .up_to_n_times(1)
                .expect(1)
                .mount(upstream)
                .await;
            let mut body =
                json!({"ItemId": aggregate, "PlaySessionId": play_session, "PositionTicks": 123});
            if include_source {
                body["MediaSourceId"] = json!(source.virtual_media_id);
            }
            let response = f
                .request(reqwest::Method::POST, endpoint, &f.user, "tv")
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "{endpoint}");
        }
        assert!(f
            .state
            .play_sessions
            .get_session_for_user(&play_session, &f.user.id)
            .await
            .is_none());
        // Direct stream clients can establish the same exact member binding
        // without a preceding PlaybackInfo negotiation.
        let direct_session = format!("direct-{index}");
        let direct = f.request(reqwest::Method::GET,
            &format!("/Videos/{aggregate}/stream.mp4?MediaSourceId={}&PlaySessionId={direct_session}", source.virtual_media_id),
            &f.user, "tv").send().await.unwrap();
        assert_eq!(direct.status(), StatusCode::OK);
        assert_eq!(direct.text().await.unwrap(), "video");
        let expected_item = member.original_media_id.clone();
        Mock::given(method("POST"))
            .and(path("/Sessions/Playing/Stopped"))
            .respond_with(move |request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["ItemId"], expected_item);
                ResponseTemplate::new(204)
            })
            .up_to_n_times(1)
            .expect(1)
            .mount(upstream)
            .await;
        let stopped = f
            .request(
                reqwest::Method::POST,
                "/Sessions/Playing/Stopped",
                &f.user,
                "tv",
            )
            .json(&json!({"ItemId": aggregate, "PlaySessionId": direct_session}))
            .send()
            .await
            .unwrap();
        assert_eq!(stopped.status(), StatusCode::NO_CONTENT);
    }
    upstream.verify().await;
}

#[tokio::test]
async fn successful_mutations_refresh_cached_user_data_and_live_feeds_always_refresh() {
    let f = Fixture::new().await;
    healthy(&f).await;
    let played = Arc::new(AtomicBool::new(false));
    for upstream in &f.upstreams {
        let played = played.clone();
        Mock::given(method("GET")).respond_with(move |request: &wiremock::Request| {
            if request.url.path() == "/System/Info/Public" {
                return ResponseTemplate::new(200).set_body_json(json!({"ServerName": "Backend"}));
            }
            let is_played = played.load(Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": "00000000000000000000000000000001", "Type": "Movie", "Name": "Movie", "UserData": {"Played": is_played, "PlaybackPositionTicks": 0, "PlayCount": 0, "IsFavorite": false, "Key": "movie", "ItemId": "00000000000000000000000000000001"}}],
                "TotalRecordCount": 1, "StartIndex": 0
            }))
        }).mount(upstream).await;
    }
    let first = catalog(&f, "/Items?Recursive=true").await;
    let id = first["Items"][0]["Id"].as_str().unwrap();
    assert_eq!(first["Items"][0]["UserData"]["Played"], false);
    let before = f.upstreams[0].received_requests().await.unwrap().len();
    assert_eq!(catalog(&f, "/Items?Recursive=true").await, first);
    assert_eq!(
        f.upstreams[0].received_requests().await.unwrap().len(),
        before
    );
    for upstream in &f.upstreams {
        let played = played.clone();
        Mock::given(method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                played.store(true, Ordering::SeqCst);
                ResponseTemplate::new(204)
            })
            .mount(upstream)
            .await;
    }
    let response = f
        .request(
            reqwest::Method::POST,
            &format!("/Users/{}/PlayedItems/{id}", f.user.id),
            &f.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        catalog(&f, "/Items?Recursive=true").await["Items"][0]["UserData"]["Played"],
        true
    );
    for endpoint in ["/Shows/NextUp", "/UserItems/Resume"] {
        played.store(false, Ordering::SeqCst);
        assert_eq!(
            catalog(&f, endpoint).await["Items"][0]["UserData"]["Played"],
            false
        );
        played.store(true, Ordering::SeqCst);
        assert_eq!(
            catalog(&f, endpoint).await["Items"][0]["UserData"]["Played"],
            true
        );
    }
}

#[tokio::test]
async fn offline_authorized_sources_retain_sightings_and_recover_without_identity_changes() {
    let f = Fixture::new().await;
    f.state.config.write().await.deduplicate_media = true;
    healthy(&f).await;
    let group = f
        .state
        .virtual_library_service
        .create_group("Movies")
        .await
        .unwrap();
    for server in f.state.server_storage.list_servers().await.unwrap() {
        f.state
            .virtual_library_service
            .add_member(&group.virtual_id, server.id, "library", "Movies", "movies")
            .await
            .unwrap();
    }
    let response = || {
        ResponseTemplate::new(200).set_body_json(json!({
        "Items": [{"Id": "00000000000000000000000000000001", "Type": "Movie", "Name": "Movie", "ProviderIds": {"Tmdb": "42"}}],
        "TotalRecordCount": 1, "StartIndex": 0
    }))
    };
    for upstream in &f.upstreams {
        Mock::given(path("/Items"))
            .respond_with(response())
            .mount(upstream)
            .await;
    }
    let query = format!("/Items?ParentId={}&Recursive=true", group.virtual_id);
    let first = catalog(&f, &query).await;
    let id = first["Items"][0]["Id"].as_str().unwrap();
    assert_eq!(first["Items"][0]["MediaSourceCount"], 2);
    f.upstreams[1].reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&f.upstreams[1])
        .await;
    f.state.server_storage.check_servers_health().await;
    let degraded = catalog(&f, &query).await;
    assert_eq!(degraded["Items"][0]["Id"], id);
    assert_eq!(degraded["Items"][0]["MediaSourceCount"], 1);
    assert_eq!(
        f.state
            .media_storage
            .get_media_version_members_by_virtual_id(id)
            .await
            .unwrap()
            .len(),
        2
    );
    f.upstreams[1].reset().await;
    Mock::given(path("/Items"))
        .respond_with(response())
        .mount(&f.upstreams[1])
        .await;
    healthy(&f).await;
    let recovered = catalog(&f, &query).await;
    assert_eq!(recovered["Items"][0]["Id"], id);
    assert_eq!(recovered["Items"][0]["MediaSourceCount"], 2);

    // A missing device session is also unavailability, not loss of access.
    let sessions = f
        .state
        .user_authorization
        .get_user_sessions(&f.user.id, None)
        .await
        .unwrap();
    let (session, server) = sessions
        .iter()
        .find(|(_, server)| server.url.as_str().trim_end_matches('/') == f.upstreams[0].uri())
        .unwrap();
    let mapping = f
        .state
        .user_authorization
        .get_server_mapping(&f.user.id, server)
        .await
        .unwrap()
        .unwrap();
    f.state
        .user_authorization
        .delete_sessions_for_mapping(mapping.id)
        .await
        .unwrap();
    let degraded = catalog(&f, &query).await;
    assert_eq!(degraded["Items"][0]["Id"], id);
    assert_eq!(degraded["Items"][0]["MediaSourceCount"], 1);
    assert_eq!(
        f.state
            .media_storage
            .get_media_version_members_by_virtual_id(id)
            .await
            .unwrap()
            .len(),
        2
    );
    let before = f.upstreams[1].received_requests().await.unwrap().len();
    assert_eq!(catalog(&f, &query).await, degraded);
    assert!(f.upstreams[1].received_requests().await.unwrap().len() > before);
    f.state
        .user_authorization
        .store_authorization_session(
            &f.user.id,
            server,
            &Authorization::parse(&auth(&f.user, "tv")).unwrap(),
            session.jellyfin_token.clone(),
            session.original_user_id.clone(),
            None,
        )
        .await
        .unwrap();
    let recovered = catalog(&f, &query).await;
    assert_eq!(recovered["Items"][0]["Id"], id);
    assert_eq!(recovered["Items"][0]["MediaSourceCount"], 2);
}

#[tokio::test]
async fn offline_root_and_search_are_not_cached_and_all_offline_returns_unavailable() {
    let f = Fixture::new().await;
    f.state.config.write().await.deduplicate_media = true;
    healthy(&f).await;
    Mock::given(path("/Items")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "Items": [{"Id": "movie", "Type": "Movie", "Name": "Movie", "ProviderIds": {"Tmdb": "42"}}],
        "TotalRecordCount": 1, "StartIndex": 0
    }))).mount(&f.upstreams[0]).await;
    f.upstreams[1].reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&f.upstreams[1])
        .await;
    f.state.server_storage.check_servers_health().await;
    for query in ["/Items?Recursive=true", "/Items?SearchTerm=Movie"] {
        let first = catalog(&f, query).await;
        let before = f.upstreams[0].received_requests().await.unwrap().len();
        assert_eq!(catalog(&f, query).await, first);
        assert!(f.upstreams[0].received_requests().await.unwrap().len() > before);
    }
    f.upstreams[0].reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&f.upstreams[0])
        .await;
    // Both request-time failures and previously detected outages return 503.
    for known_offline in [false, true] {
        if known_offline {
            f.state.server_storage.check_servers_health().await;
        }
        let response = f
            .request(reqwest::Method::GET, "/Items?Recursive=true", &f.user, "tv")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

#[tokio::test]
async fn automatic_library_keeps_its_id_and_surviving_member_during_an_outage() {
    let f = Fixture::new().await;
    f.state.config.write().await.merge_libraries = true;
    healthy(&f).await;
    let inventory = || {
        ResponseTemplate::new(200).set_body_json(json!({
        "Items": [{"Id": "library", "Type": "CollectionFolder", "CollectionType": "movies", "Name": "Movies"}],
        "TotalRecordCount": 1, "StartIndex": 0
    }))
    };
    for upstream in &f.upstreams {
        Mock::given(path("/UserViews"))
            .respond_with(inventory())
            .mount(upstream)
            .await;
    }
    let first = catalog(&f, "/UserViews").await;
    let id = first["Items"][0]["Id"].as_str().unwrap();
    assert_eq!(first["TotalRecordCount"], 1);
    f.upstreams[1].reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&f.upstreams[1])
        .await;
    // First the endpoint fails; then the health check discovers the outage.
    assert_eq!(catalog(&f, "/UserViews").await["Items"][0]["Id"], id);
    f.state.server_storage.check_servers_health().await;
    assert_eq!(catalog(&f, "/UserViews").await["Items"][0]["Id"], id);
    let sessions = f
        .state
        .user_authorization
        .get_user_sessions(&f.user.id, None)
        .await
        .unwrap();
    let scope = crate::virtual_library_service::VirtualLibraryAccessScope::new(
        &f.user.id,
        sessions.iter().map(|(_, server)| server.id),
    );
    let crate::virtual_library_service::VirtualLibraryResolution::Resolved(resolved) = f
        .state
        .virtual_library_service
        .resolve(id, Some(&scope))
        .await
        .unwrap()
    else {
        panic!("published automatic library must remain resolvable");
    };
    assert_eq!(resolved.members.len(), 2);
    f.upstreams[1].reset().await;
    Mock::given(path("/UserViews"))
        .respond_with(inventory())
        .mount(&f.upstreams[1])
        .await;
    healthy(&f).await;
    assert_eq!(catalog(&f, "/UserViews").await["Items"][0]["Id"], id);
}
