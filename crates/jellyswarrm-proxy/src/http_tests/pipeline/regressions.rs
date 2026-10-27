//! Reproductions through HTTP, real authorization/storage, and protocol-aware backends.
use super::*;
mod catalog_correctness;
mod episode_continuity;
mod streaming_routing;
mod web_client;
use wiremock::matchers::query_param;

const MOVIE: &str = "11111111111111111111111111111111";
const SOURCE_A: &str = "22222222222222222222222222222222";
const SOURCE_B: &str = "33333333333333333333333333333333";

async fn movie_catalog(f: &Fixture) -> String {
    f.state.config.write().await.deduplicate_media = true;
    healthy(f).await;
    for (index, upstream) in f.upstreams.iter().enumerate() {
        Mock::given(method("GET"))
            .and(path("/Items"))
            .respond_with(move |request: &wiremock::Request| {
                let ids = request.url.query_pairs()
                    .filter(|(key, _)| key.eq_ignore_ascii_case("Ids"))
                    .flat_map(|(_, ids)| ids.split(',').map(str::to_string).collect::<Vec<_>>())
                    .collect::<Vec<_>>();
                let items = if ids.is_empty() || ids.iter().any(|id| id == MOVIE) {
                    vec![json!({"Id": MOVIE, "Type": "Movie", "Name": format!("Movie {index}"), "ProviderIds": {"Tmdb": "42"}})]
                } else { vec![] };
                ResponseTemplate::new(200).set_body_json(json!({"TotalRecordCount": items.len(), "StartIndex": 0, "Items": items}))
            }).mount(upstream).await;
    }
    let result = catalog(f, "/Items?SearchTerm=Movie").await;
    assert_eq!(result["TotalRecordCount"], 1);
    result["Items"][0]["Id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn regression_play_queue_lookup_preserves_requested_aggregate_ids() {
    let f = Fixture::new().await;
    let aggregate = movie_catalog(&f).await;
    let result = catalog(&f, &format!("/Items?Ids={aggregate}&Fields=MediaSources")).await;
    assert_eq!(
        result["TotalRecordCount"], 1,
        "one requested movie must produce one queue entry"
    );
    assert_eq!(result["Items"][0]["Id"], aggregate);
    assert_eq!(result["Items"][0]["MediaSourceCount"], 2);

    let members = f
        .state
        .media_storage
        .get_media_version_members_by_virtual_id(&aggregate)
        .await
        .unwrap();
    let member = &members[0].mapping.virtual_media_id;
    let result = catalog(
        &f,
        &format!("/Items?Ids={aggregate}&ids={member}&StartIndex=1&Limit=1"),
    )
    .await;
    assert_eq!(
        result["TotalRecordCount"], 2,
        "an explicitly requested member is a separate queue entry"
    );
    assert_eq!(result["Items"].as_array().unwrap().len(), 1);
    let result = catalog(&f, &format!("/Items?Ids={aggregate}&Limit=0")).await;
    assert_eq!(result["TotalRecordCount"], 1);
    assert_eq!(result["Items"], json!([]));
}

#[tokio::test]
async fn regression_filtered_playback_keeps_sibling_versions_playable() {
    let f = Fixture::new().await;
    let aggregate = movie_catalog(&f).await;
    for (index, upstream) in f.upstreams.iter().enumerate() {
        let sources = if index == 0 {
            vec![SOURCE_A, SOURCE_B]
        } else {
            vec![MOVIE]
        };
        Mock::given(method("GET")).and(path(format!("/Items/{MOVIE}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Id": MOVIE, "Type": "Movie", "Name": "Movie",
                "MediaSources": sources.iter().map(|id| json!({"Id": id, "Type": "Default"})).collect::<Vec<_>>()
            }))).mount(upstream).await;
    }
    let detail = catalog(&f, &format!("/Items/{aggregate}")).await;
    assert_eq!(detail["MediaSources"].as_array().unwrap().len(), 3);
    let (_, server) = f
        .state
        .media_storage
        .get_media_mapping_with_server(&f.items[0])
        .await
        .unwrap()
        .unwrap();
    let mut sources = Vec::new();
    for original in [SOURCE_A, SOURCE_B] {
        sources.push(
            f.state
                .media_storage
                .get_media_mapping_by_original(original, server.id)
                .await
                .unwrap()
                .unwrap()
                .virtual_media_id,
        );
    }
    // Jellyfin's ResolvePlaybackMediaSources filters the response to MediaSourceId.
    Mock::given(method("POST")).and(path(format!("/Items/{MOVIE}/PlaybackInfo")))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let source = body["MediaSourceId"].as_str().unwrap();
            assert!([SOURCE_A, SOURCE_B].contains(&source));
            ResponseTemplate::new(200).set_body_json(json!({"PlaySessionId": format!("play-{source}"), "MediaSources": [{"Id": source, "Type": "Default"}]}))
        }).mount(&f.upstreams[0]).await;
    for source in [&sources[0], &sources[1], &sources[0]] {
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
            "a prior source selection must not break another version"
        );
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["MediaSources"][0]["Id"], *source);
    }
    // Both negotiated sessions must still accept reports after switching versions.
    Mock::given(method("POST"))
        .and(path("/Sessions/Playing/Progress"))
        .respond_with(ResponseTemplate::new(204))
        .expect(2)
        .mount(&f.upstreams[0])
        .await;
    for (source, original) in sources.iter().zip([SOURCE_A, SOURCE_B]) {
        let response = f.request(reqwest::Method::POST, "/Sessions/Playing/Progress", &f.user, "tv")
            .json(&json!({"ItemId": aggregate, "MediaSourceId": source, "PlaySessionId": format!("play-{original}")}))
            .send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
}

#[tokio::test]
async fn regression_redirect_authenticates_at_the_backend() {
    let f = Fixture::new().await;
    healthy(&f).await;
    let (mapping, server) = f
        .state
        .media_storage
        .get_media_mapping_with_server(&f.items[0])
        .await
        .unwrap()
        .unwrap();
    f.state
        .server_storage
        .update_server_media_streaming_mode(server.id, MediaStreamingMode::Redirect)
        .await
        .unwrap();
    let upstream_path = format!("/Videos/{}/master.m3u8", mapping.original_media_id);
    Mock::given(path(&upstream_path))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(10)
        .mount(&f.upstreams[0])
        .await;
    Mock::given(path(&upstream_path))
        .and(query_param("ApiKey", "backend-token-0"))
        .and(query_param("Static", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_string("authenticated media"))
        .mount(&f.upstreams[0])
        .await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for headers in [
        vec![("Authorization", auth(&f.user, "tv"))],
        vec![("X-MediaBrowser-Token", f.user.virtual_key.clone())],
    ] {
        let mut request = client.get(format!(
            "{}/Videos/{}/master.m3u8?Static=true",
            f.url, f.items[0]
        ));
        for (key, value) in headers {
            request = request.header(key, value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        // A redirect cannot supply the proxy's rewritten Authorization header.
        let response = reqwest::get(response.headers()["Location"].to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "following Location alone must authenticate"
        );
        assert_eq!(response.text().await.unwrap(), "authenticated media");
    }
}

#[tokio::test]
async fn regression_playback_delivery_urls_reach_exact_subtitle_and_attachment() {
    let f = Fixture::new().await;
    healthy(&f).await;
    let (mapping, _) = f
        .state
        .media_storage
        .get_media_mapping_with_server(&f.items[0])
        .await
        .unwrap()
        .unwrap();
    let subtitle = format!(
        "/Videos/{}/{SOURCE_A}/Subtitles/3/123456789/Stream.ass",
        mapping.original_media_id
    );
    let attachment = format!(
        "/Videos/{}/{SOURCE_A}/Attachments/5",
        mapping.original_media_id
    );
    Mock::given(method("POST")).and(path(format!("/Items/{}/PlaybackInfo", mapping.original_media_id)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"PlaySessionId": "subtitle-play", "MediaSources": [{
            "Id": SOURCE_A, "MediaStreams": [{"Index": 3, "Type": "Subtitle", "DeliveryUrl": format!("{subtitle}?api_key=backend-token-0&Tag=opaque")}],
            "MediaAttachments": [{"DeliveryUrl": format!("{attachment}?api_key=backend-token-0&MediaSourceId={SOURCE_A}")}]
        }]}))).mount(&f.upstreams[0]).await;
    for resource in [&subtitle, &attachment] {
        Mock::given(method("GET"))
            .and(path(resource.as_str()))
            .and(query_param("api_key", "backend-token-0"))
            .respond_with(ResponseTemplate::new(200).set_body_string("resource"))
            .expect(1)
            .mount(&f.upstreams[0])
            .await;
    }
    let response = f
        .request(
            reqwest::Method::POST,
            &format!("/Items/{}/PlaybackInfo", f.items[0]),
            &f.user,
            "tv",
        )
        .json(&json!({"DeviceProfile": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let playback: Value = response.json().await.unwrap();
    for url in [
        playback["MediaSources"][0]["MediaStreams"][0]["DeliveryUrl"]
            .as_str()
            .unwrap(),
        playback["MediaSources"][0]["MediaAttachments"][0]["DeliveryUrl"]
            .as_str()
            .unwrap(),
    ] {
        assert!(
            !url.contains(SOURCE_A),
            "backend source must be virtualized for the client"
        );
        let response = f
            .request(reqwest::Method::GET, url, &f.user, "tv")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "failed delivery URL {url}"
        );
        assert_eq!(response.text().await.unwrap(), "resource");
    }
}

#[tokio::test]
async fn regression_explicit_episode_sort_is_numeric_before_pagination() {
    let f = Fixture::new().await;
    healthy(&f).await;
    for (index, upstream) in f.upstreams.iter().enumerate() {
        let episode = if index == 0 {
            json!({"Id": MOVIE, "Type": "Episode", "Name": "Zulu", "ParentIndexNumber": 1, "IndexNumber": 1})
        } else {
            json!({"Id": SOURCE_A, "Type": "Episode", "Name": "Alpha", "ParentIndexNumber": 1, "IndexNumber": 2})
        };
        Mock::given(path("/Items"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"Items": [episode], "TotalRecordCount": 1, "StartIndex": 0}),
                ),
            )
            .mount(upstream)
            .await;
    }
    for (order, expected) in [("Ascending", "Zulu"), ("Descending", "Alpha")] {
        let result = catalog(&f, &format!("/Items?IncludeItemTypes=Episode&SortBy=ParentIndexNumber,IndexNumber,SortName&SortOrder={order}&Limit=1")).await;
        assert_eq!(result["TotalRecordCount"], 2);
        assert!(result["Items"][0]["Name"]
            .as_str()
            .unwrap()
            .starts_with(expected));
    }
}

#[tokio::test]
async fn regression_guide_sort_is_chronological_before_pagination() {
    let f = Fixture::new().await;
    healthy(&f).await;
    // Force IDs to sort opposite to StartDate rather than relying on random UUIDs.
    let mut programs = Vec::new();
    for item in &f.items {
        programs.push(
            f.state
                .media_storage
                .get_media_mapping_with_server(item)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    programs.sort_by(|left, right| left.0.virtual_media_id.cmp(&right.0.virtual_media_id));
    for (index, (mapping, server)) in programs.iter().enumerate() {
        let upstream = f
            .upstreams
            .iter()
            .find(|upstream| server.url.as_str().trim_end_matches('/') == upstream.uri())
            .unwrap();
        let date = if index == 0 {
            "2026-10-10T11:00:00Z"
        } else {
            "2026-10-10T10:00:00Z"
        };
        Mock::given(path("/LiveTv/Programs")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Items": [{"Id": mapping.original_media_id, "Type": "Program", "StartDate": date}], "TotalRecordCount": 1, "StartIndex": 0
        }))).mount(upstream).await;
    }
    let result = catalog(&f, "/LiveTv/Programs?SortBy=StartDate&Limit=1").await;
    assert_eq!(result["TotalRecordCount"], 2);
    assert_eq!(result["Items"][0]["StartDate"], "2026-10-10T10:00:00Z");
}
