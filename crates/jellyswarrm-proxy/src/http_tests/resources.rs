//! Media bytes are contracts too: resource ownership, range requests, response
//! headers and bytes must survive, without leaking a proxy key upstream.
use super::*;
use wiremock::{
    matchers::{header, method, path},
    Mock, ResponseTemplate,
};

macro_rules! contracts {
    ($($name:ident: $route:literal;)+) => {
        pub(super) const ROUTES: &[&str] = &[$($route),+];
        $(contract_test!($name, resource_contract, $route);)+
    };
}
contracts! {
    video_direct: "/Videos/{item}/stream.mp4";
    video_matroska: "/Videos/{item}/stream.mkv";
    video_stream: "/Videos/{item}/stream";
    video_master: "/Videos/{item}/master.m3u8";
    video_variant: "/Videos/{item}/main.m3u8";
    video_hls_segment: "/Videos/{item}/hls1/main/0.ts";
    audio_universal: "/Audio/{item}/universal";
    audio_stream: "/Audio/{item}/stream";
    audio_mp3: "/Audio/{item}/stream.mp3";
    audio_master: "/Audio/{item}/master.m3u8";
    audio_variant: "/Audio/{item}/main.m3u8";
    audio_segment: "/Audio/{item}/hls1/main/0.ts";
    subtitle: "/Videos/{item}/{item}/Subtitles/2/Stream.vtt";
    attachment: "/Videos/{item}/{item}/Attachments/0";
    trickplay_manifest: "/Videos/{item}/Trickplay/320/tiles.m3u8";
    trickplay_tile: "/Videos/{item}/Trickplay/320/0.jpg";
    artwork: "/Items/{item}/Images/Primary";
    chapter_artwork: "/Items/{item}/Images/Chapter/0";
    download: "/Items/{item}/Download";
    fallback_font: "/FallbackFont/Fonts/Noto.ttf";
}
async fn resource_contract(route: &str) {
    let f = Fixture::for_scenario(Scenario::NormalServers).await;
    f.healthy().await;
    let owner = usize::from(route.contains("{item}"));
    let original = f.original(owner).await;
    let upstream_path = route.replace("{item}", &original);
    let playlist = route.ends_with(".m3u8") && !route.contains("Trickplay");
    let body = if playlist {
        b"#EXTM3U\n#EXT-X-ENDLIST\n".to_vec()
    } else {
        vec![0, 255, 13, 10]
    };
    let mock = Mock::given(method("GET")).and(path(&upstream_path));
    let mock = if playlist {
        mock.respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "application/vnd.apple.mpegurl")
                .set_body_bytes(body.clone()),
        )
    } else {
        mock.and(header("range", "bytes=2-5")).respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Type", "application/octet-stream")
                .insert_header("Content-Range", "bytes 2-5/8")
                .insert_header("Accept-Ranges", "bytes")
                .set_body_bytes([0, 255, 13, 10]),
        )
    };
    mock.expect(1).mount(&f.upstreams[owner]).await;
    let response = f
        .request(
            reqwest::Method::GET,
            &route.replace("{item}", &f.items[owner]),
            &f.user,
            "tv",
        )
        .header("Range", "bytes=2-5")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        if playlist {
            StatusCode::OK
        } else {
            StatusCode::PARTIAL_CONTENT
        },
        "{route}"
    );
    if playlist {
        assert!(!response.headers().contains_key("Content-Range"));
    } else {
        assert_eq!(response.headers()["Content-Range"], "bytes 2-5/8");
    }
    assert_eq!(response.bytes().await.unwrap().as_ref(), body);
    assert!(
        f.upstreams[1 - owner]
            .received_requests()
            .await
            .unwrap()
            .is_empty(),
        "{route}"
    );
    let requests = f.upstreams[owner].received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    if playlist {
        assert!(!requests[0].headers.contains_key("range"));
    }
    let headers = format!("{:?}", requests[0].headers);
    assert!(headers.contains(&format!("backend-token-{owner}")));
    assert!(!headers.contains(&f.user.virtual_key));
}
