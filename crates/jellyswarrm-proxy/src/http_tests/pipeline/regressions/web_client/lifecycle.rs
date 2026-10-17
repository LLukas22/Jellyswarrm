//! Verify Jellyfin Web playback lifecycle transitions and backend routing over HTTP.
use super::*;

const OPENED_SOURCE: &str = "dddddddddddddddddddddddddddddddd";

async fn open_dynamic_source() -> (Fixture, String, Value) {
    let (f, aggregate, source) = web_fixture().await;
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
    Mock::given(method("POST")).and(path("/LiveStreams/Open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"MediaSource":{"Id":OPENED_SOURCE,"Name":"1080p","LiveStreamId":"live-b","MediaStreams":[],"TranscodingUrl":format!("/Videos/{MOVIE}/master.m3u8?LiveStreamId=live-b&MediaSourceId={OPENED_SOURCE}&PlaySessionId=web-1")}})))
        .mount(&f.upstreams[1]).await;
    let opened = f
        .request(
            reqwest::Method::POST,
            &format!(
                "/LiveStreams/Open?ItemId={aggregate}&PlaySessionId=web-1&UserId={}",
                f.user.id
            ),
            &f.user,
            "tv",
        )
        .json(&json!({"OpenToken":"opaque","DeviceProfile":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(opened.status(), StatusCode::OK);
    let opened: Value = opened.json().await.unwrap();
    (f, aggregate, opened["MediaSource"].clone())
}

#[tokio::test]
async fn active_opened_source_survives_detail_refresh_and_stop_releases_it() {
    let (f, aggregate, source) = open_dynamic_source().await;
    let source_id = source["Id"].as_str().unwrap();
    let live_id = source["LiveStreamId"].as_str().unwrap();
    assert_ne!(live_id, "live-b");
    assert!(source["TranscodingUrl"]
        .as_str()
        .unwrap()
        .contains(&format!("LiveStreamId={live_id}")));
    Mock::given(method("GET"))
        .and(path(format!("/Videos/{MOVIE}/stream.mp4")))
        .and(query_param("MediaSourceId", OPENED_SOURCE))
        .and(query_param("LiveStreamId", "live-b"))
        .respond_with(ResponseTemplate::new(200).set_body_string("stream"))
        .expect(2)
        .mount(&f.upstreams[1])
        .await;
    let url = format!("/Videos/{aggregate}/stream.mp4?MediaSourceId={source_id}&LiveStreamId={live_id}&PlaySessionId=web-1");
    assert_eq!(
        f.request(reqwest::Method::GET, &url, &f.user, "tv")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    catalog(&f, &format!("/Items/{aggregate}")).await;
    assert_eq!(
        f.request(reqwest::Method::GET, &url, &f.user, "tv")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    Mock::given(method("POST"))
        .and(path("/Sessions/Playing/Progress"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let progress = f.request(reqwest::Method::POST,"/Sessions/Playing/Progress",&f.user,"tv")
        .json(&json!({"ItemId":aggregate,"MediaSourceId":source_id,"PlaySessionId":"web-1","PositionTicks":100})).send().await.unwrap();
    assert_eq!(progress.status(), StatusCode::NO_CONTENT);
    f.state.play_sessions.remove_session("web-1").await;
    catalog(&f, &format!("/Items/{aggregate}")).await;
    let group = f
        .state
        .media_storage
        .get_media_version_group(&aggregate)
        .await
        .unwrap()
        .unwrap();
    assert!(f
        .state
        .media_storage
        .get_media_version_source_route(group.id, source_id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn live_actions_follow_owner_and_close_only_releases_on_success() {
    let (f, _, source) = open_dynamic_source().await;
    let id = source["LiveStreamId"].as_str().unwrap();
    // Stop reports can precede close; the live resource has its own lifetime.
    f.state.play_sessions.remove_session("web-1").await;
    Mock::given(method("POST"))
        .and(path("/LiveStreams/MediaInfo"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["LiveStreamId"], "live-b");
            ResponseTemplate::new(200)
                .set_body_json(json!({"MediaStreams":[{"Index":1,"Type":"Audio"}]}))
        })
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let response = f
        .request(
            reqwest::Method::POST,
            "/LiveStreams/MediaInfo",
            &f.user,
            "tv",
        )
        .json(&json!({"LiveStreamId":id}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let fail = Arc::new(AtomicBool::new(true));
    let failed = fail.clone();
    Mock::given(method("POST"))
        .and(path("/LiveStreams/Close"))
        .and(query_param("LiveStreamId", "live-b"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(if failed.load(Ordering::SeqCst) {
                503
            } else {
                204
            })
        })
        .expect(2)
        .mount(&f.upstreams[1])
        .await;
    let url = format!("/LiveStreams/Close?liveStreamId={id}");
    let foreign = f
        .request(reqwest::Method::POST, &url, &f.other, "tv")
        .send()
        .await
        .unwrap();
    assert!(!foreign.status().is_success());
    assert_eq!(
        f.request(reqwest::Method::POST, &url, &f.user, "tv")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    fail.store(false, Ordering::SeqCst);
    assert_eq!(
        f.request(reqwest::Method::POST, &url, &f.user, "tv")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert!(f
        .state
        .play_sessions
        .live_stream(id, &f.user.id)
        .await
        .is_none());
    assert_eq!(
        f.request(reqwest::Method::POST, &url, &f.user, "tv")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn sdk_body_only_open_translates_ids_and_query_overrides_body() {
    let (f, aggregate, source) = web_fixture().await;
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
    Mock::given(method("POST"))
        .and(path("/LiveStreams/Open"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["ItemId"], MOVIE);
            assert_eq!(body["UserId"], "backend-user-1");
            assert_eq!(body["PlaySessionId"], "web-1");
            assert_eq!(body["OpenToken"], "opaque");
            assert_eq!(body["AudioStreamIndex"], 2);
            ResponseTemplate::new(200).set_body_json(
                json!({"MediaSource":{"Id":SOURCE_B,"LiveStreamId":"live-b","MediaStreams":[]}}),
            )
        })
        .expect(2)
        .mount(&f.upstreams[1])
        .await;
    for query in [false, true] {
        let endpoint = if query {
            format!("/LiveStreams/Open?ItemId={aggregate}&PlaySessionId=web-1")
        } else {
            "/LiveStreams/Open".into()
        };
        let response = f.request(reqwest::Method::POST,&endpoint,&f.user,"tv")
            .json(&json!({"ItemId":if query {"ignored"} else {&aggregate},"UserId":f.user.id,"PlaySessionId":if query {"ignored"} else {"web-1"},"OpenToken":"opaque","DeviceProfile":{},"AudioStreamIndex":2})).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn playback_filters_use_latest_state_before_pagination() {
    let (f, _, _) = web_fixture().await;
    let library = f
        .state
        .virtual_library_service
        .create_group("Movies")
        .await
        .unwrap();
    for server in f.state.server_storage.list_servers().await.unwrap() {
        f.state
            .virtual_library_service
            .add_member(
                &library.virtual_id,
                server.id,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "Movies",
                "movies",
            )
            .await
            .unwrap();
    }
    for (index, upstream) in f.upstreams.iter().enumerate() {
        Mock::given(method("GET")).and(wiremock::matchers::path_regex("^/(Items|Users/backend-user-[01]/Items)$"))
            .respond_with(move |request: &wiremock::Request| {
                let mut items = vec![json!({"Id":MOVIE,"Type":"Movie","Name":"Completed","ProviderIds":{"Tmdb":"42"},"UserData":if index==0 {playback_data(999000000,false,Some("2026-10-10T12:00:00Z"))} else {playback_data(0,true,Some("2026-10-11T12:00:00Z"))}}),
                    json!({"Id":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","Type":"Movie","Name":"Resumable","ProviderIds":{"Tmdb":"43"},"UserData":playback_data(123000000,false,Some("2026-10-11T11:00:00Z"))})];
                for (key,value) in request.url.query_pairs() {
                    if key.eq_ignore_ascii_case("Filters") && value.contains("IsResumable") { items.retain(|item| item["UserData"]["PlaybackPositionTicks"].as_i64().unwrap() > 0); }
                    if key.eq_ignore_ascii_case("IsPlayed") { items.retain(|item| item["UserData"]["Played"] == (value=="true")); }
                }
                ResponseTemplate::new(200).set_body_json(json!({"TotalRecordCount":items.len(),"StartIndex":0,"Items":items}))
            }).with_priority(1).mount(upstream).await;
    }
    for endpoint in [
        format!(
            "/Items?ParentId={}&Filters=IsResumable&Limit=1",
            library.virtual_id
        ),
        format!(
            "/UserItems/Resume?ParentId={}&UserId={}&Limit=1",
            library.virtual_id, f.user.id
        ),
        format!("/UserItems/Resume?UserId={}&Limit=1", f.user.id),
        format!("/Users/{}/Items/Resume?Limit=1", f.user.id),
    ] {
        let response = catalog(&f, &endpoint).await;
        assert_eq!(response["TotalRecordCount"], 1, "{endpoint}: {response}");
        assert_eq!(response["Items"][0]["Name"], "Resumable");
        assert_eq!(
            response["Items"][0]["UserData"]["PlaybackPositionTicks"],
            123000000
        );
    }
    let played = catalog(
        &f,
        &format!("/Items?ParentId={}&IsPlayed=true", library.virtual_id),
    )
    .await;
    assert_eq!(played["TotalRecordCount"], 1);
    assert_eq!(played["Items"][0]["Name"], "Completed");
    let page = catalog(
        &f,
        &format!(
            "/Items?ParentId={}&Filters=IsResumable&StartIndex=1&Limit=1",
            library.virtual_id
        ),
    )
    .await;
    assert_eq!(page["TotalRecordCount"], 1);
    assert_eq!(page["Items"], json!([]));
    for filter in ["IsUnplayed", "IsPlayed"] {
        let response = catalog(
            &f,
            &format!("/Items?ParentId={}&Filters={filter}", library.virtual_id),
        )
        .await;
        assert_eq!(response["TotalRecordCount"], 1);
        assert_eq!(
            response["Items"][0]["Name"],
            if filter == "IsPlayed" {
                "Completed"
            } else {
                "Resumable"
            }
        );
    }
}

#[tokio::test]
async fn auto_opened_stream_ids_are_distinct_and_survive_renegotiation() {
    let (f, aggregate, _) = web_fixture().await;
    let detail = catalog(&f, &format!("/Items/{aggregate}")).await;
    for (index, upstream) in f.upstreams.iter().enumerate() {
        Mock::given(method("POST")).and(path(format!("/Items/{MOVIE}/PlaybackInfo")))
            .respond_with(move |request: &wiremock::Request| {
                let payload: Value = serde_json::from_slice(&request.body).unwrap();
                if let Some(id) = payload.get("LiveStreamId") { assert_eq!(id,"shared-live-id"); }
                ResponseTemplate::new(200).set_body_json(json!({"PlaySessionId":format!("auto-{index}"),"MediaSources":[{"Id":if index==0 {SOURCE_A} else {SOURCE_B},"LiveStreamId":"shared-live-id","MediaStreams":[]}]}))
            }).with_priority(1).expect(2).mount(upstream).await;
        Mock::given(method("POST"))
            .and(path("/LiveStreams/Close"))
            .and(query_param("LiveStreamId", "shared-live-id"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(upstream)
            .await;
    }
    let mut ids = Vec::new();
    for source in detail["MediaSources"].as_array().unwrap() {
        let mut payload =
            json!({"MediaSourceId":source["Id"],"AutoOpenLiveStream":true,"DeviceProfile":{}});
        let mut live_id = None;
        for _ in 0..2 {
            let response = f
                .request(
                    reqwest::Method::POST,
                    &format!("/Items/{aggregate}/PlaybackInfo"),
                    &f.user,
                    "tv",
                )
                .json(&payload)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let playback: Value = response.json().await.unwrap();
            let id = playback["MediaSources"][0]["LiveStreamId"]
                .as_str()
                .unwrap()
                .to_string();
            if let Some(previous) = &live_id {
                assert_eq!(previous, &id);
            }
            payload["LiveStreamId"] = json!(id);
            live_id = Some(id);
        }
        ids.push(live_id.unwrap());
    }
    assert_ne!(ids[0], ids[1]);
    for id in ids {
        let response = f
            .request(reqwest::Method::POST, "/LiveStreams/Close", &f.user, "tv")
            .json(&json!({"LiveStreamId":id}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
}
