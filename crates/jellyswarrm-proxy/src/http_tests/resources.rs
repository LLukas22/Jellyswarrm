//! Media bytes are contracts too: resource ownership, range requests, response
//! headers and bytes must survive, without leaking a proxy key upstream.
use super::*;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path},
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

#[tokio::test]
async fn download_without_range_streams_before_upstream_finishes() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let f = Fixture::for_scenario(Scenario::NormalServers).await;
    f.healthy().await;
    let original = f.original(1).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, wait) = tokio::sync::oneshot::channel::<()>();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(socket.read_u8().await.unwrap());
        }
        assert!(
            !String::from_utf8_lossy(&request)
                .to_lowercase()
                .contains("\r\nrange:")
        );
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=movie.mkv\r\nAccept-Ranges: bytes\r\n\r\n\x00\xff\r\n").await.unwrap();
        // The remaining bytes cannot arrive until the client has received the
        // headers and first chunk. A buffering proxy deadlocks and times out.
        let _ = wait.await;
        socket.write_all(b"done").await.unwrap();
    });
    Mock::given(method("GET"))
        .and(path(format!("/Items/{original}/Download")))
        .and(header("accept-encoding", "identity"))
        .respond_with(
            ResponseTemplate::new(307).insert_header("Location", format!("http://{address}/file")),
        )
        .expect(1)
        .mount(&f.upstreams[1])
        .await;

    let mut response = tokio::time::timeout(
        Duration::from_secs(5),
        f.request(
            reqwest::Method::GET,
            &format!("/Items/{}/Download", f.items[1]),
            &f.user,
            "tv",
        )
        .send(),
    )
    .await
    .expect("download headers must arrive before the upstream body completes")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["Content-Length"], "8");
    assert_eq!(response.headers()["Accept-Ranges"], "bytes");
    assert_eq!(
        response.headers()["Content-Disposition"],
        "attachment; filename=movie.mkv"
    );
    let first = tokio::time::timeout(Duration::from_secs(5), response.chunk())
        .await
        .expect("initial bytes must stream immediately")
        .unwrap()
        .unwrap();
    assert_eq!(first.as_ref(), b"\x00\xff\r\n");
    release.send(()).unwrap();
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"done");
    upstream.await.unwrap();
    assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
}
