//! Client HTTP regressions for streaming routing, credentials, and playback ownership.
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

#[tokio::test]
async fn offline_explicit_sources_keep_route_specific_errors_without_sibling_traffic() {
    let (f, aggregate, sources) = playback_fixture().await;
    f.upstreams[1].reset().await;
    Mock::given(path("/System/Info/Public"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&f.upstreams[1])
        .await;
    f.state.server_storage.check_servers_health().await;
    let counts = futures_util::future::join_all(
        f.upstreams
            .iter()
            .map(|upstream| async { upstream.received_requests().await.unwrap().len() }),
    )
    .await;

    // Body selection is evaluated after the healthy aggregate representative
    // is resolved. Query selection goes through the earlier URL routing gate.
    let body_selected = f
        .request(
            reqwest::Method::POST,
            &format!("/Items/{aggregate}/PlaybackInfo"),
            &f.user,
            "tv",
        )
        .json(&json!({"MediaSourceId": sources[1], "DeviceProfile": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(body_selected.status(), StatusCode::SERVICE_UNAVAILABLE);
    for (method, endpoint) in [
        (
            reqwest::Method::POST,
            format!("/Items/{aggregate}/PlaybackInfo"),
        ),
        (
            reqwest::Method::GET,
            format!("/Videos/{aggregate}/stream.mp4"),
        ),
        (
            reqwest::Method::GET,
            format!("/Videos/{aggregate}/master.m3u8"),
        ),
    ] {
        let response = f
            .request(
                method,
                &format!("{endpoint}?MediaSourceId={}", sources[1]),
                &f.user,
                "tv",
            )
            .json(&json!({"DeviceProfile": {}}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{endpoint}");
    }
    for (upstream, count) in f.upstreams.iter().zip(counts) {
        assert_eq!(upstream.received_requests().await.unwrap().len(), count);
    }
    assert!(f
        .state
        .play_sessions
        .get_sessions_by_item_id(&aggregate)
        .await
        .is_empty());
}

#[tokio::test]
async fn ambiguous_implicit_delivery_preserves_deterministic_mapping_default() {
    let (f, aggregate, sources) = playback_fixture().await;
    let members = f
        .state
        .media_storage
        .get_media_version_members_by_virtual_id(&aggregate)
        .await
        .unwrap();
    let default = members
        .iter()
        .max_by(|left, right| {
            crate::libraries::service::compare_virtual_library_routes(
                &left.server,
                &left.mapping.original_media_id,
                &right.server,
                &right.mapping.original_media_id,
            )
        })
        .unwrap();
    let owner = f
        .upstreams
        .iter()
        .position(|upstream| default.server.url.as_str().trim_end_matches('/') == upstream.uri())
        .unwrap();
    for (index, upstream) in f.upstreams.iter().enumerate() {
        Mock::given(path(format!("/Videos/{MOVIE}/master.m3u8")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("#EXTM3U\n# backend-{index}\n#EXT-X-ENDLIST\n")),
            )
            .mount(upstream)
            .await;
    }
    // Reverse creation order as well: implicit delivery must not select the
    // newest session, the first session, or reject ambiguity as HTTP 400.
    for order in [[0, 1], [1, 0]] {
        for index in order {
            negotiate(&f, &aggregate, &sources[index]).await;
        }
        assert_eq!(
            f.state
                .play_sessions
                .get_sessions_by_item_id(&aggregate)
                .await
                .len(),
            2
        );
        let counts = futures_util::future::join_all(
            f.upstreams
                .iter()
                .map(|upstream| async { upstream.received_requests().await.unwrap().len() }),
        )
        .await;
        for resource in ["stream.mp4", "master.m3u8"] {
            let response = f
                .request(
                    reqwest::Method::GET,
                    &format!("/Videos/{aggregate}/{resource}"),
                    &f.user,
                    "tv",
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{resource}");
            let expected = if resource == "stream.mp4" {
                format!("backend-{owner}")
            } else {
                format!("#EXTM3U\n# backend-{owner}\n#EXT-X-ENDLIST\n")
            };
            assert_eq!(response.text().await.unwrap(), expected);
        }
        for (index, (upstream, count)) in f.upstreams.iter().zip(counts).enumerate() {
            let requests = upstream.received_requests().await.unwrap();
            assert_eq!(requests.len() - count, if index == owner { 2 } else { 0 });
            for request in &requests[count..] {
                let identity = format!("{:?}", request.headers);
                assert!(identity.contains(&format!("backend-token-{owner}")));
                assert!(!identity.contains(&f.user.virtual_key));
                assert!(!identity.contains(&format!("backend-token-{}", 1 - owner)));
            }
        }
    }
}

#[tokio::test]
async fn anonymous_and_backend_token_delivery_do_not_borrow_proxy_credentials() {
    let (f, _, sources) = playback_fixture().await;
    for (owner, source) in sources.iter().enumerate() {
        // Fixture::items are unrelated seed mappings, not the MOVIE mappings
        // discovered by movie_catalog. Select the actual catalog member on
        // this source's backend so the rewritten item matches the media mock.
        let (_, server) = f
            .state
            .media_storage
            .get_media_mapping_with_server(source)
            .await
            .unwrap()
            .unwrap();
        let item = f
            .state
            .media_storage
            .get_media_mapping_by_original(MOVIE, server.id)
            .await
            .unwrap()
            .unwrap()
            .virtual_media_id;
        Mock::given(path(format!("/Videos/{MOVIE}/master.m3u8")))
            .respond_with(ResponseTemplate::new(200).set_body_string("#EXTM3U\n#EXT-X-ENDLIST\n"))
            .mount(&f.upstreams[owner])
            .await;
        for token in [None, Some("external-backend-token")] {
            let counts = futures_util::future::join_all(
                f.upstreams
                    .iter()
                    .map(|upstream| async { upstream.received_requests().await.unwrap().len() }),
            )
            .await;
            // The same mapped item is permitted for delivery, but negotiation
            // still requires a proxy device authorization session.
            let playback_url = format!("{}/Items/{item}/PlaybackInfo", f.url);
            let mut playback = reqwest::Client::new()
                .post(playback_url)
                .json(&json!({"DeviceProfile": {}}));
            if let Some(token) = token {
                playback = playback.query(&[("ApiKey", token)]);
            }
            assert_eq!(
                playback.send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
            for resource in ["stream.mp4", "master.m3u8"] {
                let mut request = reqwest::Client::new().get(format!(
                    "{}/Videos/{}/{resource}?MediaSourceId={}",
                    f.url, item, source
                ));
                if let Some(token) = token {
                    request = request.query(&[("ApiKey", token)]);
                }
                let response = request.send().await.unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::OK,
                    "{resource}, token={token:?}"
                );
                assert_eq!(
                    response.text().await.unwrap(),
                    if resource == "stream.mp4" {
                        format!("backend-{owner}")
                    } else {
                        "#EXTM3U\n#EXT-X-ENDLIST\n".into()
                    }
                );
            }
            for (index, (upstream, count)) in f.upstreams.iter().zip(counts).enumerate() {
                let requests = upstream.received_requests().await.unwrap();
                assert_eq!(requests.len() - count, if index == owner { 2 } else { 0 });
                for request in &requests[count..] {
                    assert_eq!(
                        request.url.path(),
                        if request.url.path().ends_with(".mp4") {
                            format!("/Videos/{MOVIE}/stream.mp4")
                        } else {
                            format!("/Videos/{MOVIE}/master.m3u8")
                        }
                    );
                    assert!(request
                        .url
                        .query_pairs()
                        .any(|(key, value)| key == "MediaSourceId"
                            && value == if owner == 0 { SOURCE_A } else { SOURCE_B }));
                    let credentials = format!("{:?} {}", request.headers, request.url);
                    assert!(!credentials.contains(&f.user.virtual_key));
                    assert!(!credentials.contains("backend-token-0"));
                    assert!(!credentials.contains("backend-token-1"));
                    assert_eq!(
                        request
                            .url
                            .query_pairs()
                            .find_map(|(key, value)| {
                                (key == "ApiKey").then(|| value.into_owned())
                            })
                            .as_deref(),
                        token
                    );
                    assert!(!request.headers.contains_key("x-emby-token"));
                    assert!(!request.headers.contains_key("authorization"));
                }
            }
        }
    }
}

#[tokio::test]
async fn raw_upstream_id_passthrough_uses_stream_default_but_not_hls_resource_fallback() {
    let (f, _, _) = playback_fixture().await;
    // Raw upstream IDs have no virtual mapping. Legacy direct delivery permits
    // the default server, whereas HLS resource delivery requires a matched ID.
    assert!(f
        .state
        .media_storage
        .get_media_mapping_by_virtual(MOVIE)
        .await
        .unwrap()
        .is_none());
    let default = f
        .state
        .server_storage
        .get_best_server()
        .await
        .unwrap()
        .unwrap();
    let owner = f
        .upstreams
        .iter()
        .position(|upstream| default.url.as_str().trim_end_matches('/') == upstream.uri())
        .unwrap();
    for token in [None, Some("external-backend-token")] {
        let counts = futures_util::future::join_all(
            f.upstreams
                .iter()
                .map(|upstream| async { upstream.received_requests().await.unwrap().len() }),
        )
        .await;
        for (resource, status) in [
            ("stream.mp4", StatusCode::OK),
            ("master.m3u8", StatusCode::NOT_FOUND),
        ] {
            let mut request =
                reqwest::Client::new().get(format!("{}/Videos/{MOVIE}/{resource}", f.url));
            if let Some(token) = token {
                request = request.query(&[("ApiKey", token)]);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), status, "{resource}, token={token:?}");
            if status == StatusCode::OK {
                assert_eq!(response.text().await.unwrap(), format!("backend-{owner}"));
            }
        }
        for (index, (upstream, count)) in f.upstreams.iter().zip(counts).enumerate() {
            let requests = upstream.received_requests().await.unwrap();
            assert_eq!(requests.len() - count, if index == owner { 1 } else { 0 });
            for request in &requests[count..] {
                assert_eq!(request.url.path(), format!("/Videos/{MOVIE}/stream.mp4"));
                assert_eq!(
                    request
                        .url
                        .query_pairs()
                        .find_map(|(key, value)| { (key == "ApiKey").then(|| value.into_owned()) })
                        .as_deref(),
                    token
                );
                let credentials = format!("{:?} {}", request.headers, request.url);
                assert!(!credentials.contains(&f.user.virtual_key));
                assert!(!credentials.contains("backend-token-0"));
                assert!(!credentials.contains("backend-token-1"));
                assert!(!request.headers.contains_key("authorization"));
            }
        }
    }
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
        .add_session(crate::playback::sessions::PlaybackSession {
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
        let expected = if query == "PlaySessionId=foreign" {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::BAD_REQUEST
        };
        assert_eq!(response.status(), expected, "{query}");
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
