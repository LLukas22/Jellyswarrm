use super::*;

const LIBRARY_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn library_fixture() -> (Fixture, String) {
    let f = Fixture::new().await;
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
            .add_member(&group.virtual_id, server.id, LIBRARY_ID, "Movies", "movies")
            .await
            .unwrap();
    }
    (f, group.virtual_id)
}

#[tokio::test]
async fn filter_routes_union_options_and_translate_filter_ids() {
    let (f, library) = library_fixture().await;
    for (index, upstream) in f.upstreams.iter().enumerate() {
        Mock::given(method("GET")).and(path("/Items/Filters"))
            .and(query_param("ParentId", LIBRARY_ID))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Genres":[format!("Genre {index}"),"Shared"],"Tags":[format!("tag-{index}")],"Years":[2025+index],"OfficialRatings":["PG"]})))
            .mount(upstream).await;
        Mock::given(method("GET")).and(path("/Items/Filters2"))
            .and(query_param("ParentId", LIBRARY_ID))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Genres":[{"Name":format!("Genre {index}"),"Id":format!("{:032x}", index+10)}],"Tags":[format!("tag-{index}")]})))
            .mount(upstream).await;
    }
    let legacy = catalog(
        &f,
        &format!("/Items/Filters?ParentId={library}&UserId={}", f.user.id),
    )
    .await;
    assert_eq!(legacy["Genres"].as_array().unwrap().len(), 3);
    assert_eq!(legacy["Tags"].as_array().unwrap().len(), 2);
    assert_eq!(legacy["OfficialRatings"], json!(["PG"]));
    let modern = catalog(&f, &format!("/Items/Filters2?ParentId={library}")).await;
    let genres = modern["Genres"].as_array().unwrap();
    assert_eq!(genres.len(), 2);
    for genre in genres {
        assert!(f
            .state
            .media_storage
            .get_media_mapping_by_virtual(genre["Id"].as_str().unwrap())
            .await
            .unwrap()
            .is_some());
    }
    // A failed backend does not make the other member's options disappear.
    Mock::given(path("/Items/Filters2"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&f.upstreams[1])
        .await;
    assert_eq!(
        catalog(&f, &format!("/Items/Filters2?ParentId={library}")).await["Genres"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    Mock::given(path("/Items/Filters2"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&f.upstreams[0])
        .await;
    let response = f
        .request(
            reqwest::Method::GET,
            &format!("/Items/Filters2?ParentId={library}"),
            &f.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn genres_and_studios_include_all_library_members_before_pagination() {
    let (f, library) = library_fixture().await;
    for (index, upstream) in f.upstreams.iter().enumerate() {
        for (endpoint, kind) in [("/Genres", "Genre"), ("/Studios", "Studio")] {
            Mock::given(method("GET")).and(path(endpoint)).and(query_param("ParentId", LIBRARY_ID))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Items":[{"Id":format!("{:032x}",index+10),"Name":format!("{kind} {index}"),"Type":kind}],"TotalRecordCount":1,"StartIndex":0})))
                .mount(upstream).await;
        }
    }
    for endpoint in ["/Genres", "/Studios"] {
        let response = catalog(
            &f,
            &format!("{endpoint}?ParentId={library}&IncludeItemTypes=Movie&Recursive=true"),
        )
        .await;
        assert_eq!(response["Items"].as_array().unwrap().len(), 2);
        let page = catalog(&f, &format!("{endpoint}?ParentId={library}&IncludeItemTypes=Movie&Recursive=true&StartIndex=1&Limit=1")).await;
        assert_eq!(page["TotalRecordCount"], 2);
        assert_eq!(page["Items"].as_array().unwrap().len(), 1);
        let foreign = f
            .request(
                reqwest::Method::GET,
                &format!("{endpoint}?ParentId={library}"),
                &f.other,
                "tv",
            )
            .send()
            .await
            .unwrap();
        assert!(!foreign.status().is_success());
    }
}

#[tokio::test]
async fn studio_filter_partitions_and_translates_advertised_ids() {
    let (f, library) = library_fixture().await;
    for (index, upstream) in f.upstreams.iter().enumerate() {
        let studio = format!("{:032x}", index + 10);
        Mock::given(method("GET")).and(path("/Studios"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Items":[{"Id":studio,"Name":format!("Studio {index}"),"Type":"Studio"}],"TotalRecordCount":1,"StartIndex":0})))
            .mount(upstream).await;
        Mock::given(method("GET")).and(path("/Items"))
            .respond_with(move |request: &wiremock::Request| {
                assert!(request.url.query_pairs().any(|(key,value)| key.eq_ignore_ascii_case("StudioIds") && value == studio));
                ResponseTemplate::new(200).set_body_json(json!({"TotalRecordCount":1,"StartIndex":0,"Items":[{"Id":MOVIE,"Name":format!("Movie {index}"),"Type":"Movie"}]}))
            }).mount(upstream).await;
    }
    let studios = catalog(&f, &format!("/Studios?ParentId={library}")).await;
    let ids = studios["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Id"].as_str().unwrap())
        .collect::<Vec<_>>();
    let single = catalog(
        &f,
        &format!(
            "/Items?ParentId={library}&StudioIds={}&IncludeItemTypes=Movie&Recursive=true",
            ids[0]
        ),
    )
    .await;
    assert_eq!(single["TotalRecordCount"], 1);
    let all = catalog(&f,&format!("/Items?ParentId={library}&StudioIds={}&StudioIds={}&IncludeItemTypes=Movie&Recursive=true",ids[0],ids[1])).await;
    assert_eq!(all["TotalRecordCount"], 2);
    let unknown = catalog(
        &f,
        &format!(
            "/Items?ParentId={library}&StudioIds={}",
            uuid::Uuid::new_v4().simple()
        ),
    )
    .await;
    assert_eq!(unknown["TotalRecordCount"], 0);
}

#[tokio::test]
async fn livestream_open_follows_negotiation_and_records_new_source() {
    let (f, aggregate, source) = web_fixture().await;
    Mock::given(method("POST")).and(path(format!("/Items/{MOVIE}/PlaybackInfo")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "PlaySessionId":"open-secondary", "MediaSources":[{"Id":SOURCE_B,"Name":"Remote","RequiresOpening":true,"OpenToken":"opaque-backend-1-token","MediaStreams":[]}]
        }))).with_priority(1).mount(&f.upstreams[1]).await;
    let playback = f
        .request(
            reqwest::Method::POST,
            &format!("/Items/{aggregate}/PlaybackInfo"),
            &f.user,
            "tv",
        )
        .json(&json!({"MediaSourceId":source,"DeviceProfile":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(playback.status(), StatusCode::OK);
    let playback: Value = playback.json().await.unwrap();
    Mock::given(method("POST"))
        .and(path("/LiveStreams/Open"))
        .respond_with(ResponseTemplate::new(404))
        .expect(0)
        .mount(&f.upstreams[0])
        .await;
    let opened_id = "dddddddddddddddddddddddddddddddd";
    Mock::given(method("POST")).and(path("/LiveStreams/Open"))
        .and(query_param("ItemId", MOVIE)).and(query_param("PlaySessionId", "open-secondary"))
        .and(query_param("UserId", "backend-user-1"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["OpenToken"],"opaque-backend-1-token");
            ResponseTemplate::new(200).set_body_json(json!({"MediaSource":{"Id":opened_id,"Name":"Remote","LiveStreamId":"live-b","MediaStreams":[]}}))
        }).expect(1).mount(&f.upstreams[1]).await;
    let endpoint = format!("/LiveStreams/Open?UserId={}&ItemId={aggregate}&PlaySessionId=open-secondary&StartTimeTicks=0", f.user.id);
    let response = f
        .request(reqwest::Method::POST, &endpoint, &f.user, "tv")
        .json(&json!({"DeviceProfile":{},"OpenToken":playback["MediaSources"][0]["OpenToken"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let opened: Value = response.json().await.unwrap();
    assert_eq!(
        opened["MediaSource"]["Name"],
        playback["MediaSources"][0]["Name"]
    );
    let opened_source = opened["MediaSource"]["Id"].as_str().unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/Videos/{MOVIE}/stream.mp4")))
        .and(query_param("MediaSourceId", opened_id))
        .respond_with(ResponseTemplate::new(200).set_body_string("opened stream"))
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let stream = f.request(reqwest::Method::GET,&format!("/Videos/{aggregate}/stream.mp4?MediaSourceId={opened_source}&PlaySessionId=open-secondary"),&f.user,"tv").send().await.unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    assert_eq!(stream.text().await.unwrap(), "opened stream");
    for endpoint in [
        endpoint.replace("open-secondary", "unknown"),
        endpoint.replace(&aggregate, &f.items[0]),
    ] {
        let response = f
            .request(reqwest::Method::POST, &endpoint, &f.user, "tv")
            .json(&json!({"OpenToken":"opaque-backend-1-token"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let foreign = f
        .request(reqwest::Method::POST, &endpoint, &f.other, "tv")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert!(!foreign.status().is_success());
    Mock::given(path("/System/Info/Public"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&f.upstreams[1])
        .await;
    f.state.server_storage.check_servers_health().await;
    let unavailable = f
        .request(reqwest::Method::POST, &endpoint, &f.user, "tv")
        .json(&json!({"OpenToken":"opaque-backend-1-token"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn watched_actions_apply_to_all_copies_and_survive_reload() {
    let (f, aggregate, _) = web_fixture().await;
    let played = [
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicBool::new(true)),
    ];
    for (index, upstream) in f.upstreams.iter().enumerate() {
        let value = played[index].clone();
        Mock::given(method("GET")).and(path(format!("/Items/{MOVIE}")))
            .respond_with(move |_: &wiremock::Request| ResponseTemplate::new(200).set_body_json(json!({
                "Id":MOVIE,"Type":"Movie","Name":"Movie","MediaSources":[{"Id":if index==0 {SOURCE_A} else {SOURCE_B}}],
                "UserData":playback_data(0,value.load(Ordering::SeqCst),if index==0 {None} else {Some("2026-10-11T12:00:00Z")})
            }))).with_priority(1).mount(upstream).await;
        for endpoint in [
            format!("/Users/backend-user-{index}/PlayedItems/{MOVIE}"),
            format!("/UserPlayedItems/{MOVIE}"),
        ] {
            for (verb, target) in [("POST", true), ("DELETE", false)] {
                let value = played[index].clone();
                Mock::given(method(verb))
                    .and(path(&endpoint))
                    .respond_with(move |_: &wiremock::Request| {
                        value.store(target, Ordering::SeqCst);
                        ResponseTemplate::new(200).set_body_json(playback_data(0, target, None))
                    })
                    .expect(1)
                    .mount(upstream)
                    .await;
            }
        }
    }
    for endpoint in [
        format!("/Users/{}/PlayedItems/{aggregate}", f.user.id),
        format!("/UserPlayedItems/{aggregate}?UserId={}", f.user.id),
    ] {
        for (verb, target) in [
            (reqwest::Method::DELETE, false),
            (reqwest::Method::POST, true),
        ] {
            let response = f
                .request(verb, &endpoint, &f.user, "tv")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let result: Value = response.json().await.unwrap();
            assert_eq!(result["ItemId"], aggregate);
            for state in &played {
                assert_eq!(state.load(Ordering::SeqCst), target);
            }
            assert_eq!(
                catalog(&f, &format!("/Items/{aggregate}")).await["UserData"]["Played"],
                target
            );
        }
    }
}

#[tokio::test]
async fn watched_actions_preflight_offline_members() {
    let (f, aggregate, _) = web_fixture().await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200).set_body_json(playback_data(0, false, None)))
        .expect(0)
        .mount(&f.upstreams[0])
        .await;
    Mock::given(path("/System/Info/Public"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&f.upstreams[1])
        .await;
    f.state.server_storage.check_servers_health().await;
    let response = f
        .request(
            reqwest::Method::DELETE,
            &format!("/Users/{}/PlayedItems/{aggregate}", f.user.id),
            &f.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn watched_actions_propagate_partial_failures_and_keep_member_actions_local() {
    for explicit_member in [false, true] {
        let (f, aggregate, _) = web_fixture().await;
        let (_, server) = f
            .state
            .media_storage
            .get_media_mapping_with_server(&f.items[0])
            .await
            .unwrap()
            .unwrap();
        let member = f
            .state
            .media_storage
            .get_media_mapping_by_original(MOVIE, server.id)
            .await
            .unwrap()
            .unwrap();
        Mock::given(method("DELETE"))
            .and(path(format!("/Users/backend-user-0/PlayedItems/{MOVIE}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(playback_data(0, false, None)))
            .expect(1)
            .mount(&f.upstreams[0])
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("/Users/backend-user-1/PlayedItems/{MOVIE}")))
            .respond_with(ResponseTemplate::new(403))
            .expect(if explicit_member { 0 } else { 1 })
            .mount(&f.upstreams[1])
            .await;
        let id = if explicit_member {
            &member.virtual_media_id
        } else {
            &aggregate
        };
        let response = f
            .request(
                reqwest::Method::DELETE,
                &format!("/Users/{}/PlayedItems/{id}", f.user.id),
                &f.user,
                "tv",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if explicit_member {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            }
        );
    }
}
