//! Follow only client-visible identities from library discovery to media bytes.
//! Unlike the focused regressions, no library groups or media/source mappings
//! are inserted directly into storage to make these workflows pass.
use super::*;
use wiremock::matchers::{header, query_param};

const ITEM: &str = "11111111111111111111111111111111";
const SOURCE: &str = "22222222222222222222222222222222";

#[derive(Clone, Copy)]
enum Media {
    Movie,
    Episode,
    Audio,
}

impl Media {
    fn kind(self) -> &'static str {
        match self {
            Self::Movie => "Movie",
            Self::Episode => "Episode",
            Self::Audio => "Audio",
        }
    }

    fn collection(self) -> &'static str {
        match self {
            Self::Movie => "movies",
            Self::Episode => "tvshows",
            Self::Audio => "music",
        }
    }

    fn route(self) -> &'static str {
        match self {
            Self::Movie | Self::Episode => "Videos",
            Self::Audio => "Audio",
        }
    }

    fn media_type(self) -> &'static str {
        if matches!(self, Self::Audio) {
            "Audio"
        } else {
            "Video"
        }
    }

    fn collapses(self, scenario: Scenario) -> bool {
        scenario.merged() && !matches!(self, Self::Audio)
    }
}

scenario_test!(
    movie_proxy,
    media_path_contract,
    Media::Movie,
    MediaStreamingMode::Proxy
);
scenario_test!(
    movie_redirect,
    media_path_contract,
    Media::Movie,
    MediaStreamingMode::Redirect
);
scenario_test!(
    episode_proxy,
    media_path_contract,
    Media::Episode,
    MediaStreamingMode::Proxy
);
scenario_test!(
    episode_redirect,
    media_path_contract,
    Media::Episode,
    MediaStreamingMode::Redirect
);
scenario_test!(
    audio_proxy,
    media_path_contract,
    Media::Audio,
    MediaStreamingMode::Proxy
);
scenario_test!(
    audio_redirect,
    media_path_contract,
    Media::Audio,
    MediaStreamingMode::Redirect
);

fn list(items: Vec<Value>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({"TotalRecordCount":items.len(),"StartIndex":0,"Items":items}))
}

async fn mount_media(f: &Fixture, media: Media, mode: MediaStreamingMode) {
    for server in f.state.server_storage.list_servers().await.unwrap() {
        f.state
            .server_storage
            .update_server_media_streaming_mode(server.id, mode)
            .await
            .unwrap();
    }
    for (owner, upstream) in f.upstreams.iter().enumerate() {
        // Deliberately reuse item/source IDs across servers: only proxy IDs and
        // the selected source/session can distinguish the two copies.
        let library = format!("{:032x}", owner + 100);
        let tracks = json!([{"Type":"Audio","Index":1,"Codec":"aac","Language":"eng"}]);
        let source = json!({
            "Id":SOURCE,"Name":format!("Source {owner}"),"Type":"Default",
            "SupportsDirectPlay":true,"SupportsDirectStream":true,"SupportsTranscoding":true,
            "MediaStreams":tracks,"DefaultAudioStreamIndex":1
        });
        let item = json!({
            "Id":ITEM,"Name":"Shared title","Type":media.kind(),"MediaType":media.media_type(),
            "ProviderIds":{"Tmdb":"42"},"ParentIndexNumber":1,"IndexNumber":1,
            "Overview":"Media details","RunTimeTicks":600000000,
            "MediaSources":[source.clone()],"MediaStreams":tracks,
            "Chapters":[{"Name":"Opening","StartPositionTicks":0}],
            "UserData":{"ItemId":ITEM,"Played":false,"PlaybackPositionTicks":0,"PlayCount":0,"IsFavorite":false,"Key":"media"}
        });
        Mock::given(method("GET")).and(path("/UserViews"))
            .respond_with(list(vec![json!({"Id":library,"Name":"Library","Type":"CollectionFolder","CollectionType":media.collection()})]))
            .mount(upstream).await;
        Mock::given(method("GET"))
            .and(path("/Items"))
            .and(query_param("ParentId", &library))
            .respond_with(list(vec![item.clone()]))
            .mount(upstream)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/Items/{ITEM}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(item))
            .mount(upstream)
            .await;
        let route = media.route();
        let session = format!("play-{owner}");
        Mock::given(method("POST"))
            .and(path(format!("/Items/{ITEM}/PlaybackInfo")))
            .respond_with(move |request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["MediaSourceId"], SOURCE);
                assert_eq!(body["UserId"], format!("backend-user-{owner}"));
                assert_eq!(body["AudioStreamIndex"], 1);
                assert_eq!(body["IsPlayback"], true);
                assert_eq!(body["DeviceProfile"]["Name"], "Web contract");
                let mut source = source.clone();
                source["TranscodingUrl"] = json!(format!(
                    "/{route}/{ITEM}/master.m3u8?MediaSourceId={SOURCE}&PlaySessionId={session}&ApiKey=backend-token-{owner}&DeviceId=tv"
                ));
                ResponseTemplate::new(200)
                    .set_body_json(json!({"PlaySessionId":session,"MediaSources":[source]}))
            })
            .mount(upstream)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/{route}/{ITEM}/stream")))
            .and(query_param("MediaSourceId", SOURCE))
            .and(header("range", "bytes=2-5"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Type", "application/octet-stream")
                    .insert_header("Content-Range", "bytes 2-5/8")
                    .insert_header("Accept-Ranges", "bytes")
                    .set_body_bytes([owner as u8, 255, 13, 10]),
            )
            .expect(1)
            .mount(upstream)
            .await;
        let query = format!("MediaSourceId={SOURCE}&PlaySessionId=play-{owner}&ApiKey=backend-token-{owner}&DeviceId=tv&AudioStreamIndex=1&Tag=opaque%26tag");
        for (resource, content_type, bytes) in [
            (
                "master.m3u8",
                "application/vnd.apple.mpegurl",
                format!("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000\nmain.m3u8?{query}\n")
                    .into_bytes(),
            ),
            (
                "main.m3u8",
                "application/vnd.apple.mpegurl",
                format!("#EXTM3U\n#EXTINF:4.0,\nhls1/main/0.ts?{query}\n#EXT-X-ENDLIST\n")
                    .into_bytes(),
            ),
            (
                "hls1/main/0.ts",
                "video/mp2t",
                vec![0x47, owner as u8, 0, 255],
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/{route}/{ITEM}/{resource}")))
                .and(query_param("PlaySessionId", format!("play-{owner}")))
                .and(query_param("MediaSourceId", SOURCE))
                .and(query_param("ApiKey", format!("backend-token-{owner}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("Content-Type", content_type)
                        .set_body_bytes(bytes),
                )
                .expect(1)
                .mount(upstream)
                .await;
        }
        for endpoint in [
            "/Sessions/Playing",
            "/Sessions/Playing/Progress",
            "/Sessions/Playing/Stopped",
        ] {
            Mock::given(method("POST"))
                .and(path(endpoint))
                .respond_with(move |request: &wiremock::Request| {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    assert_eq!(body["ItemId"], ITEM);
                    assert_eq!(body["MediaSourceId"], SOURCE);
                    assert_eq!(body["PlaySessionId"], format!("play-{owner}"));
                    assert_eq!(body["PositionTicks"], 123000);
                    ResponseTemplate::new(204)
                })
                .expect(1)
                .mount(upstream)
                .await;
        }
    }
}

async fn request_counts(f: &Fixture) -> Vec<usize> {
    let mut counts = Vec::new();
    for upstream in &f.upstreams {
        counts.push(upstream.received_requests().await.unwrap().len());
    }
    counts
}

async fn delivery(
    f: &Fixture,
    url: &str,
    range: Option<&str>,
    owner: usize,
    mode: MediaStreamingMode,
) -> reqwest::Response {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut request = client.get(url).header("Authorization", auth(&f.user, "tv"));
    if let Some(range) = range {
        request = request.header("Range", range);
    }
    let response = request.send().await.unwrap();
    if mode == MediaStreamingMode::Redirect {
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        let location = response.headers()["Location"].to_str().unwrap();
        assert!(location.starts_with(&format!("{}/", f.upstreams[owner].uri())));
        assert!(!location.contains(&f.user.virtual_key));
        // Follow Location alone: browsers cannot attach rewritten backend auth.
        let mut request = client.get(location);
        if let Some(range) = range {
            request = request.header("Range", range);
        }
        request.send().await.unwrap()
    } else {
        assert!(!response.status().is_redirection());
        response
    }
}

async fn media_path_contract(scenario: Scenario, media: Media, mode: MediaStreamingMode) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    mount_media(&f, media, mode).await;
    let roots = catalog(&f, &format!("/UserViews?UserId={}", f.user.id)).await;
    assert_eq!(
        roots["TotalRecordCount"],
        if scenario.merged() { 1 } else { 2 }
    );
    let mut ids = std::collections::BTreeSet::new();
    for library in roots["Items"].as_array().unwrap() {
        let before = request_counts(&f).await;
        let items = catalog(&f, &format!("/Items?ParentId={}&UserId={}&Recursive=true&IncludeItemTypes={}&Fields=MediaSources", library["Id"].as_str().unwrap(), f.user.id, media.kind())).await;
        let after = request_counts(&f).await;
        assert_eq!(
            after.iter().zip(&before).filter(|(a, b)| a > b).count(),
            if scenario.merged() { 2 } else { 1 }
        );
        let expected = if scenario.merged() && !media.collapses(scenario) {
            2
        } else {
            1
        };
        assert_eq!(items["TotalRecordCount"], expected);
        for item in items["Items"].as_array().unwrap() {
            assert_ne!(item["Id"], ITEM);
            ids.insert(item["Id"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(ids.len(), if media.collapses(scenario) { 1 } else { 2 });

    let mut played_owners = std::collections::BTreeSet::new();
    // reqwest otherwise copies the full query into cross-origin Referer headers,
    // unlike a browser's strict-origin-when-cross-origin default.
    let player = reqwest::Client::builder().referer(false).build().unwrap();
    for id in ids {
        let detail = catalog(
            &f,
            &format!(
                "/Items/{id}?UserId={}&Fields=MediaSources,Chapters",
                f.user.id
            ),
        )
        .await;
        assert_eq!(detail["Id"], id);
        assert_eq!(detail["Type"], media.kind());
        assert_eq!(detail["MediaType"], media.media_type());
        assert_eq!(detail["Overview"], "Media details");
        assert_eq!(detail["Chapters"][0]["Name"], "Opening");
        let sources = detail["MediaSources"].as_array().unwrap();
        assert_eq!(sources.len(), if media.collapses(scenario) { 2 } else { 1 });
        for source in sources {
            let owner = (0..2)
                .find(|owner| {
                    source["Name"]
                        .as_str()
                        .unwrap()
                        .contains(&format!("Source {owner}"))
                })
                .unwrap();
            assert!(
                played_owners.insert(owner),
                "each backend copy must be played exactly once"
            );
            let source_id = source["Id"].as_str().unwrap();
            assert_ne!(source_id, SOURCE);
            assert_eq!(source["MediaStreams"][0]["Codec"], "aac");
            let before = request_counts(&f).await;
            let response = f.request(reqwest::Method::POST, &format!("/Items/{id}/PlaybackInfo"), &f.user, "tv")
                .json(&json!({"UserId":f.user.id,"MediaSourceId":source_id,"AudioStreamIndex":1,"IsPlayback":true,"DeviceProfile":{"Name":"Web contract"}}))
                .send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let playback: Value = response.json().await.unwrap();
            assert_eq!(playback["PlaySessionId"], format!("play-{owner}"));
            assert_eq!(playback["MediaSources"][0]["Id"], source_id);
            assert_eq!(playback["MediaSources"][0]["DefaultAudioStreamIndex"], 1);
            let session = playback["PlaySessionId"].as_str().unwrap();

            let response = delivery(
                &f,
                &format!(
                    "{}/{}/{id}/stream?MediaSourceId={source_id}&PlaySessionId={session}",
                    f.url,
                    media.route()
                ),
                Some("bytes=2-5"),
                owner,
                mode,
            )
            .await;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers()["Content-Range"], "bytes 2-5/8");
            assert_eq!(response.headers()["Accept-Ranges"], "bytes");
            assert_eq!(
                response.bytes().await.unwrap().as_ref(),
                [owner as u8, 255, 13, 10]
            );

            // Consume the negotiated URL, not an independently manufactured ID.
            let transcoding = playback["MediaSources"][0]["TranscodingUrl"]
                .as_str()
                .unwrap();
            assert!(!transcoding.contains(ITEM));
            assert!(!transcoding.contains(SOURCE));
            let mut url = url::Url::parse(&format!("{}{transcoding}", f.url)).unwrap();
            for (next, expected_type) in [
                (Some("main.m3u8"), "application/vnd.apple.mpegurl"),
                (Some("hls1/main/0.ts"), "application/vnd.apple.mpegurl"),
                (None, "video/mp2t"),
            ] {
                // Native video/Hls.js follows redirects and resolves child URIs
                // against the final response URL. No Authorization or query repairs.
                let response = player.get(url.clone()).send().await.unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{url}");
                assert_eq!(response.headers()["Content-Type"], expected_type);
                let base = response.url().clone();
                let expected_origin = if mode == MediaStreamingMode::Proxy {
                    &f.url
                } else {
                    &f.upstreams[owner].uri()
                };
                assert!(base.as_str().starts_with(expected_origin));
                let bytes = response.bytes().await.unwrap();
                if let Some(next) = next {
                    let manifest = std::str::from_utf8(&bytes).unwrap();
                    assert!(manifest.starts_with("#EXTM3U\n"));
                    if mode == MediaStreamingMode::Proxy {
                        assert!(
                            !manifest.contains("backend-token-"),
                            "backend credentials leaked: {manifest}"
                        );
                        assert!(
                            !manifest.contains(SOURCE),
                            "backend source ID leaked: {manifest}"
                        );
                    }
                    let reference = manifest
                        .lines()
                        .find(|line| !line.starts_with('#'))
                        .unwrap();
                    url = base.join(reference).unwrap();
                    assert!(url.path().ends_with(next));
                    assert!(url
                        .query_pairs()
                        .any(|(key, value)| key == "PlaySessionId" && value == session));
                    assert!(url
                        .query_pairs()
                        .any(|(key, value)| key == "Tag" && value == "opaque&tag"));
                    assert!(url
                        .query_pairs()
                        .any(|(key, value)| key == "AudioStreamIndex" && value == "1"));
                } else {
                    assert_eq!(bytes.as_ref(), [0x47, owner as u8, 0, 255]);
                }
            }
            for endpoint in [
                "/Sessions/Playing",
                "/Sessions/Playing/Progress",
                "/Sessions/Playing/Stopped",
            ] {
                let response = f.request(reqwest::Method::POST, endpoint, &f.user, "tv")
                    .json(&json!({"ItemId":id,"MediaSourceId":source_id,"PlaySessionId":session,"PositionTicks":123000}))
                    .send().await.unwrap();
                assert_eq!(response.status(), StatusCode::NO_CONTENT, "{endpoint}");
            }
            let after = request_counts(&f).await;
            assert_eq!(
                after[1 - owner],
                before[1 - owner],
                "unselected backend must receive no playback requests"
            );
            assert_eq!(after[owner] - before[owner], 8);

            let before = request_counts(&f).await;
            for (verb, route) in [
                (reqwest::Method::GET, format!("/Items/{id}")),
                (reqwest::Method::POST, format!("/Items/{id}/PlaybackInfo")),
                (
                    reqwest::Method::GET,
                    format!("/{}/{id}/stream?MediaSourceId={source_id}", media.route()),
                ),
            ] {
                let response = f
                    .request(verb, &route, &f.other, "tv")
                    .json(&json!({"MediaSourceId":source_id,"DeviceProfile":{}}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::FORBIDDEN,
                    "foreign viewer: {route}"
                );
            }
            assert_eq!(request_counts(&f).await, before);
        }
    }
    assert_eq!(played_owners.len(), 2);
    for (owner, upstream) in f.upstreams.iter().enumerate() {
        upstream.verify().await;
        for request in upstream.received_requests().await.unwrap() {
            let identity = format!("{:?} {}", request.headers, request.url);
            assert!(identity.contains(&format!("backend-token-{owner}")));
            assert!(!identity.contains(&f.user.virtual_key));
        }
    }
}
