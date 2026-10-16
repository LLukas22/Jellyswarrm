//! Playlist references are consumed as a player would: URL authentication only.
use super::*;
use wiremock::{
    matchers::{method, path},
    Mock, ResponseTemplate,
};

scenario_test!(
    playlist_uri_attributes_and_reference_styles,
    references_contract
);
async fn references_contract(scenario: Scenario) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    let original = f.original(0).await;
    let root = format!("/Videos/{original}");
    let token = "ApiKey=backend-token-0&Tag=a%26b&DeviceId=tv";
    let manifest = format!(
        "#EXTM3U\r\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a,b\",URI=\"{root}/audio.m3u8?{token}\"\r\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"{}/Videos/{original}/key?{token}\"\r\n\
         #EXT-X-MAP:URI=\"//{}/Videos/{original}/init.mp4?{token}\",BYTERANGE=\"4@0\"\r\n\
         #EXT-X-SESSION-DATA:DATA-ID=\"external\",URI=\"https://cdn.example/metadata.json?signature=keep\"\r\n\
         #EXTINF:4.0,\r\n  hls1/main/0.ts?Tag=a%26b&DeviceId=tv  \r\n#EXT-X-ENDLIST\r\n",
        f.upstreams[0].uri(), f.upstreams[0].address(),
    );
    Mock::given(method("GET"))
        .and(path(format!("{root}/master.m3u8")))
        .respond_with(move |request: &wiremock::Request| {
            assert_eq!(request.headers["accept-encoding"], "identity");
            for name in ["range", "if-range", "if-none-match", "if-modified-since"] {
                assert!(!request.headers.contains_key(name), "{name}");
            }
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "application/vnd.apple.mpegurl")
                .insert_header("ETag", "\"backend-version\"")
                .insert_header("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT")
                .insert_header("Accept-Ranges", "bytes")
                .set_body_bytes(manifest.as_bytes())
        })
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    let response = f
        .request(
            reqwest::Method::GET,
            &format!("/Videos/{}/master.m3u8", f.items[0]),
            &f.user,
            "tv",
        )
        .header("Accept-Encoding", "gzip")
        .header("Range", "bytes=0-100")
        .header("If-Range", "\"old\"")
        .header("If-None-Match", "\"old\"")
        .header("If-Modified-Since", "Wed, 21 Oct 2015 07:28:00 GMT")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["Cache-Control"], "private, no-store");
    for name in [
        "etag",
        "last-modified",
        "accept-ranges",
        "content-range",
        "content-encoding",
    ] {
        assert!(!response.headers().contains_key(name), "{name}");
    }
    let base = response.url().clone();
    let text = response.text().await.unwrap();
    assert!(!text.contains("backend-token"));
    assert!(!text.contains(&original));
    assert!(text.contains("GROUP-ID=\"a,b\""));
    assert!(text.contains("BYTERANGE=\"4@0\""));
    assert!(text.contains("https://cdn.example/metadata.json?signature=keep"));
    assert!(text.ends_with("  \r\n#EXT-X-ENDLIST\r\n"));
    let attributes = regex::Regex::new(r#"URI="([^"]*)""#).unwrap();
    let mut references: Vec<_> = attributes
        .captures_iter(&text)
        .map(|capture| capture[1].to_string())
        .collect();
    references.push(
        text.lines()
            .find(|line| !line.starts_with('#'))
            .unwrap()
            .trim()
            .to_string(),
    );
    for reference in references {
        let url = base.join(&reference).unwrap();
        if url.host_str() == Some("cdn.example") {
            continue;
        }
        assert_eq!(url.origin(), base.origin());
        assert!(url.path().contains(&f.items[0]));
        assert!(url
            .query_pairs()
            .any(|(key, value)| key == "ApiKey" && value == f.user.virtual_key));
        assert!(url
            .query_pairs()
            .any(|(key, value)| key == "Tag" && value == "a&b"));
        let upstream_path = f
            .state
            .remove_prefix_from_path(url.path())
            .await
            .replace(&f.items[0], &original);
        let body = if url.path().ends_with(".m3u8") {
            b"#EXTM3U\n#EXT-X-ENDLIST\n".to_vec()
        } else {
            vec![0, 255, 13, 10]
        };
        Mock::given(method("GET"))
            .and(path(upstream_path))
            .and(wiremock::matchers::query_param("ApiKey", "backend-token-0"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/octet-stream")
                    .set_body_bytes(body.clone()),
            )
            .expect(1)
            .mount(&f.upstreams[0])
            .await;
        let response = reqwest::get(url.clone()).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{url}: {:?}",
            f.upstreams[0].received_requests().await.unwrap()
        );
        assert_eq!(response.bytes().await.unwrap().as_ref(), body);
    }
    assert!(f.upstreams[1].received_requests().await.unwrap().is_empty());
    Mock::given(method("HEAD"))
        .and(path(format!("{root}/master.m3u8")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "application/vnd.apple.mpegurl")
                .insert_header("ETag", "\"unrewritten\"")
                .insert_header("Content-Length", "1000"),
        )
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    let response = f
        .request(
            reqwest::Method::HEAD,
            &format!("/Videos/{}/master.m3u8", f.items[0]),
            &f.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("etag"));
    assert_ne!(
        response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok()),
        Some("1000")
    );
    assert!(response.bytes().await.unwrap().is_empty());
}

scenario_test!(
    playlist_invalid_upstream_responses_fail_closed,
    invalid_contract
);
async fn invalid_contract(scenario: Scenario) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    let original = f.original(0).await;
    for (name, template, expected) in [
        (
            "invalid",
            ResponseTemplate::new(200).set_body_string("not a playlist"),
            StatusCode::BAD_GATEWAY,
        ),
        (
            "partial",
            ResponseTemplate::new(206).set_body_string("#EXTM3U\nbackend-token"),
            StatusCode::BAD_GATEWAY,
        ),
        (
            "cached",
            ResponseTemplate::new(304),
            StatusCode::BAD_GATEWAY,
        ),
        (
            "compressed",
            ResponseTemplate::new(200)
                .insert_header("Content-Encoding", "gzip")
                .set_body_bytes([0, 1, 2]),
            StatusCode::BAD_GATEWAY,
        ),
        (
            "error",
            ResponseTemplate::new(403).set_body_string("backend-token"),
            StatusCode::FORBIDDEN,
        ),
        (
            "oversized",
            ResponseTemplate::new(200)
                .set_body_string(format!("#EXTM3U\n{}", "x".repeat(8 * 1024 * 1024))),
            StatusCode::BAD_GATEWAY,
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/Videos/{original}/{name}.m3u8")))
            .respond_with(template)
            .expect(1)
            .mount(&f.upstreams[0])
            .await;
        let response = f
            .request(
                reqwest::Method::GET,
                &format!("/Videos/{}/{name}.m3u8", f.items[0]),
                &f.user,
                "tv",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{name}");
        assert!(!response.text().await.unwrap().contains("backend-token"));
    }
}

scenario_test!(
    playlist_content_type_is_recognized_without_m3u8_path,
    content_type_contract
);
async fn content_type_contract(scenario: Scenario) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    let original = f.original(0).await;
    for (route, mime) in [
        ("Audio/{item}/universal", "audio/x-mpegurl; charset=utf-8"),
        ("Videos/{item}/stream.mp4", "application/x-mpegurl"),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{}", route.replace("{item}", &original))))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", mime)
                    .set_body_bytes(b"#EXTM3U\nmain.m3u8?ApiKey=backend-token-0&DeviceId=tv\n"),
            )
            .expect(1)
            .mount(&f.upstreams[0])
            .await;
        let response = f
            .request(
                reqwest::Method::GET,
                &format!("/{}", route.replace("{item}", &f.items[0])),
                &f.user,
                "tv",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let base = response.url().clone();
        let text = response.text().await.unwrap();
        assert!(!text.contains("backend-token"));
        let child = base.join(text.lines().nth(1).unwrap()).unwrap();
        assert!(child.path().contains(&f.items[0]));
        assert!(child.path().starts_with("/proxy/"));
        assert!(child
            .query_pairs()
            .any(|(key, value)| key == "ApiKey" && value == f.user.virtual_key));
    }
    assert!(f.upstreams[1].received_requests().await.unwrap().is_empty());
}
