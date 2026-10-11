use super::*;
mod routes;

async fn web_fixture() -> (Fixture, String, String) {
    let f = Fixture::new().await;
    let aggregate = movie_catalog(&f).await;
    for (index, upstream) in f.upstreams.iter().enumerate() {
        let source = if index == 0 { SOURCE_A } else { SOURCE_B };
        Mock::given(method("GET")).and(path(format!("/Items/{MOVIE}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Id": MOVIE, "Type": "Movie", "Name": "Movie", "MediaType": "Video",
                "MediaStreams": [{"Type": "Video", "Index": 0}, {"Type": "Audio", "Index": 1, "Language": "eng", "Codec": "aac"}],
                "MediaSources": [{"Id": source, "Name": "1080p", "Type": "Default", "MediaStreams": [{"Type": "Audio", "Index": 1, "Language": "eng", "Codec": "aac"}]}]
            }))).mount(upstream).await;
        Mock::given(method("POST")).and(path(format!("/Items/{MOVIE}/PlaybackInfo")))
            .respond_with(move |request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                // Model Jellyfin's source filtering, including rejecting IDs
                // which belong to another backend or to the proxy aggregate.
                if body["MediaSourceId"].as_str().is_some_and(|id| id != source) {
                    return ResponseTemplate::new(400);
                }
                ResponseTemplate::new(200).set_body_json(json!({
                    "PlaySessionId": format!("web-{index}"), "MediaSources": [{"Id": source, "Name": "1080p", "DefaultAudioStreamIndex": 1, "MediaStreams": [{"Type": "Audio", "Index": 1, "Language": "eng", "Codec": "aac"}]}]
                }))
            }).mount(upstream).await;
    }
    catalog(&f, &format!("/Items/{aggregate}")).await;
    let (_, server) = f
        .state
        .media_storage
        .get_media_mapping_with_server(&f.items[1])
        .await
        .unwrap()
        .unwrap();
    let source = f
        .state
        .media_storage
        .get_media_mapping_by_original(SOURCE_B, server.id)
        .await
        .unwrap()
        .unwrap()
        .virtual_media_id;
    (f, aggregate, source)
}

#[tokio::test]
async fn web_remembered_track_fallback_accepts_item_as_source() {
    let (f, aggregate, _) = web_fixture().await;
    // playbackmanager.js uses item.Id when remembered tracks were matched
    // but no version was explicitly selected or matched by name.
    for source in [
        aggregate.clone(),
        uuid::Uuid::parse_str(&aggregate)
            .unwrap()
            .hyphenated()
            .to_string()
            .to_uppercase(),
    ] {
        let response = f.request(reqwest::Method::POST, &format!("/Items/{aggregate}/PlaybackInfo"), &f.user, "tv")
            .json(&json!({"UserId":f.user.id,"MediaSourceId":source,"AudioStreamIndex":1,"SubtitleStreamIndex":-1,"IsPlayback":true,"AutoOpenLiveStream":true,"DeviceProfile":{}}))
            .send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let playback: Value = response.json().await.unwrap();
        assert_eq!(playback["PlaySessionId"], "web-0");
    }
    let requests = f.upstreams[0].received_requests().await.unwrap();
    let negotiations = requests
        .iter()
        .filter(|r| r.url.path().ends_with("/PlaybackInfo"))
        .collect::<Vec<_>>();
    assert_eq!(negotiations.len(), 2);
    for request in negotiations {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert!(body.get("MediaSourceId").is_none());
        assert_eq!(body["AudioStreamIndex"], 1);
        assert_eq!(body["SubtitleStreamIndex"], -1);
    }

    // Only the requested aggregate itself has default-source semantics.
    let response = f
        .request(
            reqwest::Method::POST,
            &format!("/Items/{aggregate}/PlaybackInfo"),
            &f.user,
            "tv",
        )
        .json(
            &json!({"MediaSourceId":uuid::Uuid::new_v4().simple().to_string(),"DeviceProfile":{}}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn web_track_fallback_pins_native_or_default_source_among_versions() {
    for (sources, expected) in [
        (vec![SOURCE_A, MOVIE], MOVIE),
        (vec![SOURCE_A, SOURCE_B], SOURCE_A),
    ] {
        let (f, aggregate, _) = web_fixture().await;
        Mock::given(method("POST"))
            .and(path(format!("/Items/{MOVIE}/PlaybackInfo")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "PlaySessionId":"native-fallback",
                "MediaSources":sources.iter().map(|id| json!({"Id":id,"Name":"1080p","MediaStreams":[]})).collect::<Vec<_>>()
            })))
            .with_priority(1).mount(&f.upstreams[0]).await;
        let response = f
            .request(
                reqwest::Method::POST,
                &format!("/Items/{aggregate}/PlaybackInfo"),
                &f.user,
                "tv",
            )
            .json(&json!({"MediaSourceId":aggregate,"AudioStreamIndex":1,"DeviceProfile":{}}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let playback: Value = response.json().await.unwrap();
        assert_eq!(playback["MediaSources"].as_array().unwrap().len(), 1);
        let id = playback["MediaSources"][0]["Id"].as_str().unwrap();
        let mapping = f
            .state
            .media_storage
            .get_media_mapping_by_virtual(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mapping.original_media_id, expected);
    }
}

#[tokio::test]
async fn web_version_name_survives_playback_negotiation() {
    let (f, aggregate, _) = web_fixture().await;
    let detail = catalog(&f, &format!("/Items/{aggregate}")).await;
    for advertised in detail["MediaSources"].as_array().unwrap() {
        let source = advertised["Id"].as_str().unwrap();
        // Repeat negotiation to cover stream changes as well as initial play.
        for _ in 0..2 {
            let response = f.request(reqwest::Method::POST, &format!("/Items/{aggregate}/PlaybackInfo"), &f.user, "tv")
                .json(&json!({"UserId":f.user.id,"MediaSourceId":source,"IsPlayback":true,"DeviceProfile":{}})).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let playback: Value = response.json().await.unwrap();
            // getMatchingMediaSource uses the negotiated Name to select the
            // corresponding source from the next queued item's details.
            assert_eq!(playback["MediaSources"][0]["Name"], advertised["Name"]);
            assert_eq!(playback["MediaSources"][0]["Id"], source);
        }
    }
}

fn playback_data(position: i64, played: bool, date: Option<&str>) -> Value {
    json!({"PlaybackPositionTicks":position,"Played":played,"PlayCount":1,"IsFavorite":false,"Key":"movie","ItemId":MOVIE,"LastPlayedDate":date,"PlayedPercentage":if played {100.0} else {20.0}})
}

async fn mount_user_data(f: &Fixture, data: [Value; 2]) {
    for (index, (upstream, data)) in f.upstreams.iter().zip(data).enumerate() {
        let item = json!({
            "Id":MOVIE,"Type":"Movie","Name":"Movie","ProviderIds":{"Tmdb":"42"},
            "MediaSources":[{"Id":if index == 0 { SOURCE_A } else { SOURCE_B }}],
            "UserData":data
        });
        Mock::given(method("GET"))
            .and(path(format!("/Items/{MOVIE}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(item.clone()))
            .with_priority(1)
            .mount(upstream)
            .await;
        Mock::given(method("GET"))
            .and(path("/Items"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"Items":[item],"TotalRecordCount":1,"StartIndex":0})),
            )
            .with_priority(1)
            .mount(upstream)
            .await;
    }
}

#[tokio::test]
async fn web_detail_reflects_secondary_copy_playback_progress() {
    let (f, aggregate, source) = web_fixture().await;
    let response = f
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
    assert_eq!(response.status(), StatusCode::OK);
    Mock::given(method("POST"))
        .and(path("/Sessions/Playing/Progress"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["ItemId"], MOVIE);
            assert_eq!(body["MediaSourceId"], SOURCE_B);
            ResponseTemplate::new(204)
        })
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let response = f.request(reqwest::Method::POST, "/Sessions/Playing/Progress", &f.user, "tv")
        .json(&json!({"ItemId":aggregate,"MediaSourceId":source,"PlaySessionId":"web-1","PositionTicks":123000000,"EventName":"TimeUpdate"}))
        .send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    mount_user_data(
        &f,
        [
            playback_data(0, false, None),
            playback_data(123000000, false, Some("2026-10-11T12:00:00Z")),
        ],
    )
    .await;
    let detail = catalog(&f, &format!("/Items/{aggregate}")).await;
    assert_eq!(detail["UserData"]["PlaybackPositionTicks"], 123000000);
    assert_eq!(detail["UserData"]["ItemId"], aggregate);
    let queue = catalog(&f, &format!("/Items?Ids={aggregate}")).await;
    assert_eq!(queue["Items"][0]["UserData"], detail["UserData"]);
    let listing = catalog(&f, "/Items?SearchTerm=Movie").await;
    assert_eq!(listing["Items"][0]["UserData"], detail["UserData"]);
}

#[tokio::test]
async fn web_resume_uses_latest_playback_not_largest_position() {
    for (position, played) in [(0, true), (100, false)] {
        let (f, aggregate, _) = web_fixture().await;
        let mut primary = playback_data(999000000, false, Some("2026-10-11T12:00:00+02:00"));
        primary["IsFavorite"] = json!(true);
        mount_user_data(
            &f,
            [
                primary,
                playback_data(position, played, Some("2026-10-11T11:00:00Z")),
            ],
        )
        .await;
        for result in [
            catalog(&f, &format!("/Items/{aggregate}")).await,
            catalog(&f, &format!("/Items?Ids={aggregate}")).await["Items"][0].clone(),
        ] {
            assert_eq!(result["UserData"]["PlaybackPositionTicks"], position);
            assert_eq!(result["UserData"]["Played"], played);
            assert_eq!(result["UserData"]["IsFavorite"], true);
            assert_eq!(result["UserData"]["LastPlayedDate"], "2026-10-11T11:00:00Z");
        }
    }
}

#[tokio::test]
async fn web_resume_falls_back_to_played_copy_when_dates_are_missing() {
    let (f, aggregate, _) = web_fixture().await;
    let mut primary = playback_data(0, false, None);
    primary["PlayCount"] = json!(0);
    mount_user_data(&f, [primary, playback_data(123000000, false, None)]).await;
    let detail = catalog(&f, &format!("/Items/{aggregate}")).await;
    assert_eq!(detail["UserData"]["PlaybackPositionTicks"], 123000000);
}

#[tokio::test]
async fn web_explicit_member_retains_its_own_resume_state() {
    let (f, aggregate, secondary_source) = web_fixture().await;
    mount_user_data(
        &f,
        [
            playback_data(100, false, Some("2026-10-11T12:00:00Z")),
            playback_data(200, false, Some("2026-10-10T12:00:00Z")),
        ],
    )
    .await;
    let merged = catalog(&f, &format!("/Items/{aggregate}")).await;
    assert_eq!(merged["UserData"]["PlaybackPositionTicks"], 100);
    let selected = catalog(&f, &format!("/Items/{secondary_source}")).await;
    assert_eq!(selected["UserData"]["PlaybackPositionTicks"], 200);
}
