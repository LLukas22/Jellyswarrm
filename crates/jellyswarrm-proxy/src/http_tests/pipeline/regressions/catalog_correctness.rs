use super::*;

#[tokio::test]
async fn queue_sources_are_playable_without_detail() {
    let f = Fixture::new().await;
    let aggregate = movie_catalog(&f).await;
    for upstream in &f.upstreams {
        upstream.reset().await;
        Mock::given(method("GET")).and(path("/Items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": MOVIE, "Type": "Movie", "Name": "Movie", "MediaSources": [{"Id": SOURCE_A, "Type": "Default"}]}],
                "TotalRecordCount": 1, "StartIndex": 0
            }))).mount(upstream).await;
        Mock::given(method("POST")).and(path(format!("/Items/{MOVIE}/PlaybackInfo")))
            .respond_with(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["MediaSourceId"], SOURCE_A);
                ResponseTemplate::new(200).set_body_json(json!({
                    "PlaySessionId": "queue-play", "MediaSources": [{"Id": SOURCE_A, "Type": "Default"}]
                }))
            }).mount(upstream).await;
    }
    healthy(&f).await;
    let queue = catalog(&f, &format!("/Items?Ids={aggregate}&Fields=MediaSources")).await;
    assert_eq!(queue["Items"][0]["Id"], aggregate);
    let source = queue["Items"][0]["MediaSources"][0]["Id"].as_str().unwrap();
    let response = f
        .request(
            reqwest::Method::POST,
            &format!("/Items/{aggregate}/PlaybackInfo"),
            &f.user,
            "tv",
        )
        .json(&json!({"MediaSourceId": source, "DeviceProfile": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "source advertised by queue lookup must be playable"
    );
    let playback: Value = response.json().await.unwrap();
    assert_eq!(playback["MediaSources"][0]["Id"], source);
}

#[tokio::test]
async fn movie_inventory_removes_deleted_member_and_preserves_other_types() {
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
    let removed = Arc::new(AtomicBool::new(false));
    for (index, upstream) in f.upstreams.iter().enumerate() {
        let removed = removed.clone();
        Mock::given(path("/Items")).respond_with(move |request: &wiremock::Request| {
            let only_movies = request.url.query_pairs().any(|(key, value)| key == "IncludeItemTypes" && value == "Movie");
            let mut items = if index == 0 && removed.load(Ordering::SeqCst) { vec![] } else {
                vec![json!({"Id": MOVIE, "Type": "Movie", "Name": "Movie", "ProviderIds": {"Tmdb": "42"}})]
            };
            if !only_movies {
                items.push(json!({"Id": SOURCE_B, "Type": "Series", "Name": "Series", "ProviderIds": {"Tmdb": "43"}}));
            }
            ResponseTemplate::new(200).set_body_json(json!({"TotalRecordCount": items.len(), "StartIndex": 0, "Items": items}))
        }).mount(upstream).await;
        Mock::given(path(format!("/Items/{MOVIE}")))
            .respond_with(if index == 0 {
                ResponseTemplate::new(404)
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(json!({"Id": MOVIE, "Type": "Movie", "Name": "Movie"}))
            })
            .mount(upstream)
            .await;
        Mock::given(path(format!("/Items/{MOVIE}/PlaybackInfo")))
            .respond_with(if index == 0 {
                ResponseTemplate::new(404)
            } else {
                ResponseTemplate::new(200).set_body_json(
                    json!({"PlaySessionId": "surviving-copy", "MediaSources": [{"Id": MOVIE}]}),
                )
            })
            .mount(upstream)
            .await;
    }
    let full = catalog(
        &f,
        &format!("/Items?ParentId={}&Recursive=true", group.virtual_id),
    )
    .await;
    let series = full["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["Type"] == "Series")
        .unwrap()["Id"]
        .as_str()
        .unwrap();
    let query = format!(
        "/Items?ParentId={}&Recursive=true&IncludeItemTypes=Movie",
        group.virtual_id
    );
    let first = catalog(&f, &query).await;
    let aggregate = first["Items"][0]["Id"].as_str().unwrap();
    removed.store(true, Ordering::SeqCst);
    f.state.catalog_snapshots.invalidate_viewer(&f.user.id);
    let refreshed = catalog(&f, &query).await;
    assert_eq!(refreshed["Items"][0]["Id"], aggregate);
    assert_eq!(refreshed["Items"][0]["MediaSourceCount"], 1);
    assert_eq!(
        f.state
            .media_storage
            .get_media_version_members_by_virtual_id(series)
            .await
            .unwrap()
            .len(),
        2
    );
    let response = f
        .request(
            reqwest::Method::GET,
            &format!("/Items/{aggregate}"),
            &f.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "refreshed inventory must route to surviving copy"
    );
    let response = f
        .request(
            reqwest::Method::POST,
            &format!("/Items/{aggregate}/PlaybackInfo"),
            &f.user,
            "tv",
        )
        .json(&json!({"DeviceProfile": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["PlaySessionId"],
        "surviving-copy"
    );
}

#[tokio::test]
async fn offline_id_does_not_block_other_lookup_results() {
    let f = Fixture::new().await;
    healthy(&f).await;
    let (mapping, _) = f
        .state
        .media_storage
        .get_media_mapping_with_server(&f.items[0])
        .await
        .unwrap()
        .unwrap();
    Mock::given(path("/Items")).and(query_param("Ids", mapping.original_media_id.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Items": [{"Id": mapping.original_media_id, "Type": "Movie", "Name": "Online movie"}], "TotalRecordCount": 1, "StartIndex": 0
        }))).mount(&f.upstreams[0]).await;
    f.upstreams[1].reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&f.upstreams[1])
        .await;
    f.state.server_storage.check_servers_health().await;
    let before = f.upstreams[1].received_requests().await.unwrap().len();
    for ids in [
        format!("{},{}", f.items[1], f.items[0]),
        format!("{},{}", f.items[0], f.items[1]),
    ] {
        let result = catalog(&f, &format!("/Items?Ids={ids}")).await;
        assert_eq!(result["TotalRecordCount"], 1);
        assert_eq!(result["Items"][0]["Id"], f.items[0]);
    }
    assert_eq!(
        f.upstreams[1].received_requests().await.unwrap().len(),
        before
    );
    let response = f
        .request(
            reqwest::Method::GET,
            &format!("/Items?Ids={}", f.items[1]),
            &f.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn batch_lookup_does_not_forward_unauthorized_ids() {
    let f = Fixture::new().await;
    healthy(&f).await;
    let (online, _) = f
        .state
        .media_storage
        .get_media_mapping_with_server(&f.items[0])
        .await
        .unwrap()
        .unwrap();
    let (_, foreign_server) = f
        .state
        .media_storage
        .get_media_mapping_with_server(&f.items[1])
        .await
        .unwrap()
        .unwrap();
    let mapping = f
        .state
        .user_authorization
        .get_server_mapping(&f.user.id, &foreign_server)
        .await
        .unwrap()
        .unwrap();
    f.state
        .user_authorization
        .delete_server_mapping(mapping.id)
        .await
        .unwrap();
    Mock::given(path("/Items")).and(query_param("Ids", online.original_media_id.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Items": [{"Id": online.original_media_id, "Type": "Movie"}], "TotalRecordCount": 1, "StartIndex": 0
        }))).expect(1).mount(&f.upstreams[0]).await;
    let before = f.upstreams[1].received_requests().await.unwrap().len();
    let response = catalog(&f, &format!("/Items?Ids={},{}", f.items[1], f.items[0])).await;
    assert_eq!(response["TotalRecordCount"], 1);
    assert_eq!(response["Items"][0]["Id"], f.items[0]);
    assert_eq!(
        f.upstreams[1].received_requests().await.unwrap().len(),
        before
    );
}
