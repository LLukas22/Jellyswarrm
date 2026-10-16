use super::*;

async fn playback_fixture() -> (Fixture, String, Vec<String>) {
    let f = Fixture::new().await;
    let aggregate = movie_catalog(&f).await;
    for (index, upstream) in f.upstreams.iter().enumerate() {
        let source = if index == 0 { SOURCE_A } else { SOURCE_B };
        Mock::given(method("GET")).and(path(format!("/Items/{MOVIE}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Id": MOVIE, "Name": "Movie", "Type": "Movie", "MediaSources": [{"Id": source, "Type": "Default"}]
            }))).mount(upstream).await;
        Mock::given(method("POST")).and(path(format!("/Items/{MOVIE}/PlaybackInfo")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "PlaySessionId": format!("play-{index}"), "MediaSources": [{"Id": source, "Type": "Default"}]
            }))).mount(upstream).await;
        Mock::given(method("GET"))
            .and(path(format!("/Videos/{MOVIE}/stream.mp4")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("backend-{index}")))
            .mount(upstream)
            .await;
    }
    catalog(&f, &format!("/Items/{aggregate}")).await;
    let mut sources = Vec::new();
    for (index, id) in f.items.iter().enumerate() {
        let (_, server) = f
            .state
            .media_storage
            .get_media_mapping_with_server(id)
            .await
            .unwrap()
            .unwrap();
        sources.push(
            f.state
                .media_storage
                .get_media_mapping_by_original(
                    if index == 0 { SOURCE_A } else { SOURCE_B },
                    server.id,
                )
                .await
                .unwrap()
                .unwrap()
                .virtual_media_id,
        );
    }
    (f, aggregate, sources)
}

async fn negotiate(f: &Fixture, aggregate: &str, source: &str) {
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
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn explicit_source_without_session_is_not_overridden_by_another_playback() {
    let (f, aggregate, sources) = playback_fixture().await;
    negotiate(&f, &aggregate, &sources[0]).await;
    let original = f
        .state
        .play_sessions
        .get_session_for_user("play-0", &f.user.id)
        .await
        .unwrap();
    for resource in ["stream.mp4", "master.m3u8"] {
        if resource == "master.m3u8" {
            Mock::given(path(format!("/Videos/{MOVIE}/master.m3u8")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string("#EXTM3U\n# backend-1\n#EXT-X-ENDLIST\n"),
                )
                .mount(&f.upstreams[1])
                .await;
        }
        let response = f
            .request(
                reqwest::Method::GET,
                &format!(
                    "/Videos/{aggregate}/{resource}?MediaSourceId={}",
                    sources[1]
                ),
                &f.user,
                "tv",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.text().await.unwrap(),
            if resource == "master.m3u8" {
                "#EXTM3U\n# backend-1\n#EXT-X-ENDLIST\n"
            } else {
                "backend-1"
            }
        );
    }
    assert_eq!(
        f.state
            .play_sessions
            .get_session_for_user("play-0", &f.user.id)
            .await
            .unwrap()
            .server_id,
        original.server_id
    );
}

#[tokio::test]
async fn subtitle_and_attachment_path_sources_route_to_their_owner() {
    let (f, aggregate, sources) = playback_fixture().await;
    for resource in ["Subtitles/0/Stream.srt", "Attachments/0"] {
        Mock::given(method("GET"))
            .and(path(format!("/Videos/{MOVIE}/{SOURCE_B}/{resource}")))
            .respond_with(ResponseTemplate::new(200).set_body_string("resource B"))
            .expect(1)
            .mount(&f.upstreams[1])
            .await;
        let response = f
            .request(
                reqwest::Method::GET,
                &format!("/Videos/{aggregate}/{}/{resource}", sources[1]),
                &f.user,
                "tv",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "resource B");
    }
}

#[tokio::test]
async fn stop_transcoding_uses_play_session_backend() {
    let (f, aggregate, sources) = playback_fixture().await;
    negotiate(&f, &aggregate, &sources[1]).await;
    for upstream in &f.upstreams {
        Mock::given(method("DELETE"))
            .and(path("/Videos/ActiveEncodings"))
            .respond_with(ResponseTemplate::new(204))
            .mount(upstream)
            .await;
    }
    let response = f
        .request(
            reqwest::Method::DELETE,
            "/Videos/ActiveEncodings?DeviceId=tv&PlaySessionId=play-1",
            &f.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let wrong = f.upstreams[0]
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.method == "DELETE")
        .count();
    let correct = f.upstreams[1]
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.method == "DELETE")
        .count();
    assert_eq!((wrong, correct), (0, 1));
    // Quality changes stop an encoding while retaining the playback identity.
    assert!(f
        .state
        .play_sessions
        .get_session_for_user("play-1", &f.user.id)
        .await
        .is_some());
}

#[tokio::test]
async fn explicit_sessions_keep_two_versions_separate_and_preserve_ranges() {
    let (f, aggregate, sources) = playback_fixture().await;
    for source in &sources {
        negotiate(&f, &aggregate, source).await;
    }
    for (index, source) in sources.iter().enumerate() {
        Mock::given(method("GET"))
            .and(path(format!("/Videos/{MOVIE}/stream.mp4")))
            .and(wiremock::matchers::header("Range", "bytes=10-19"))
            .and(query_param(
                "MediaSourceId",
                if index == 0 { SOURCE_A } else { SOURCE_B },
            ))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 10-19/100")
                    .set_body_string("0123456789"),
            )
            .with_priority(1)
            .mount(&f.upstreams[index])
            .await;
        let response = f.request(reqwest::Method::GET, &format!("/Videos/{aggregate}/stream.mp4?MediaSourceId={source}&PlaySessionId=play-{index}"), &f.user, "tv")
            .header("Range", "bytes=10-19").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()["Content-Range"], "bytes 10-19/100");
        assert_eq!(response.text().await.unwrap(), "0123456789");
    }
}

#[tokio::test]
async fn equivalent_aggregate_uuids_keep_negotiated_backend_and_binding() {
    let (f, aggregate, sources) = playback_fixture().await;
    negotiate(&f, &aggregate, &sources[1]).await;
    let original = f
        .state
        .play_sessions
        .get_session_for_user("play-1", &f.user.id)
        .await
        .unwrap();
    for id in [
        uuid::Uuid::parse_str(&aggregate)
            .unwrap()
            .hyphenated()
            .to_string(),
        aggregate.to_ascii_uppercase(),
    ] {
        for resource in ["stream.mp4", "master.m3u8"] {
            if resource == "master.m3u8" {
                Mock::given(path(format!("/Videos/{MOVIE}/master.m3u8")))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_string("#EXTM3U\n# backend-1\n#EXT-X-ENDLIST\n"),
                    )
                    .mount(&f.upstreams[1])
                    .await;
            }
            let response = f
                .request(
                    reqwest::Method::GET,
                    &format!("/Videos/{id}/{resource}?PlaySessionId=play-1"),
                    &f.user,
                    "tv",
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.text().await.unwrap(),
                if resource == "master.m3u8" {
                    "#EXTM3U\n# backend-1\n#EXT-X-ENDLIST\n"
                } else {
                    "backend-1"
                }
            );
        }
    }
    assert_eq!(
        f.state
            .play_sessions
            .get_session_for_user("play-1", &f.user.id)
            .await
            .unwrap()
            .server_id,
        original.server_id
    );
}

#[tokio::test]
async fn conflicting_stream_authorities_are_rejected_before_forwarding() {
    let (f, aggregate, sources) = playback_fixture().await;
    negotiate(&f, &aggregate, &sources[0]).await;
    let counts = futures_util::future::join_all(
        f.upstreams
            .iter()
            .map(|upstream| async { upstream.received_requests().await.unwrap().len() }),
    )
    .await;
    for url in [
        format!(
            "/Videos/{aggregate}/stream.mp4?PlaySessionId=play-0&MediaSourceId={}",
            sources[1]
        ),
        format!(
            "/Videos/{aggregate}/{}/Subtitles/0/Stream.srt?MediaSourceId={}",
            sources[1], sources[0]
        ),
        format!(
            "/Videos/{aggregate}/{}/Subtitles/0/Stream.srt?PlaySessionId=play-0",
            sources[1]
        ),
        format!("/Videos/{aggregate}/{}/Attachments/0", f.items[1]),
        format!("/Videos/{}/stream.mp4?PlaySessionId=play-0", f.items[0]),
        format!("/Videos/{aggregate}/stream.mp4?PlaySessionId=play-0&SessionId=other"),
    ] {
        let response = f
            .request(reqwest::Method::GET, &url, &f.user, "tv")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{url}");
    }
    for (upstream, count) in f.upstreams.iter().zip(counts) {
        assert_eq!(upstream.received_requests().await.unwrap().len(), count);
    }
    assert_eq!(
        f.state
            .play_sessions
            .get_session_for_user("play-0", &f.user.id)
            .await
            .unwrap()
            .original_item_id
            .as_deref(),
        Some(MOVIE)
    );
}

#[tokio::test]
async fn cancellation_rejects_unknown_foreign_and_unavailable_sessions() {
    let (f, aggregate, sources) = playback_fixture().await;
    negotiate(&f, &aggregate, &sources[1]).await;
    let (_, server) = f
        .state
        .media_storage
        .get_media_mapping_with_server(&sources[1])
        .await
        .unwrap()
        .unwrap();
    f.state
        .play_sessions
        .add_session(crate::session_storage::PlaybackSession {
            session_id: "foreign".into(),
            item_id: aggregate.clone(),
            user_id: f.other.id.clone(),
            server_id: server.id,
            original_item_id: Some(MOVIE.into()),
        })
        .await;
    for query in [
        "DeviceId=tv",
        "SessionId=play-1",
        "PlaySessionId=unknown",
        "PlaySessionId=foreign",
        "PlaySessionId=play-1&SessionId=other",
    ] {
        let response = f
            .request(
                reqwest::Method::DELETE,
                &format!("/Videos/ActiveEncodings?{query}"),
                &f.user,
                "tv",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
    }
    let foreign_stream = f
        .request(
            reqwest::Method::GET,
            &format!("/Videos/{aggregate}/stream.mp4?PlaySessionId=foreign"),
            &f.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(foreign_stream.status(), StatusCode::FORBIDDEN);
    f.upstreams[1].reset().await;
    Mock::given(path("/System/Info/Public"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&f.upstreams[1])
        .await;
    f.state.server_storage.check_servers_health().await;
    let response = f
        .request(
            reqwest::Method::DELETE,
            "/Videos/ActiveEncodings?PlaySessionId=play-1",
            &f.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    for upstream in &f.upstreams {
        assert!(upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method != "DELETE"));
    }
}
