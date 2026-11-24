//! #224: the catch-all must not forward unauthenticated probes to a backend.
use hyper::StatusCode;
use wiremock::{matchers::any, Mock, ResponseTemplate};

async fn requests_to(fixture: &super::support::Fixture, path: &str) -> usize {
    let mut count = 0;
    for upstream in &fixture.upstreams {
        count += upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == path)
            .count();
    }
    count
}

#[tokio::test]
async fn unauthenticated_probes_are_answered_locally() {
    let fixture = super::support::Fixture::new().await;
    fixture.healthy().await;
    let client = reqwest::Client::new();
    for path in [
        "/config.php",
        "/secrets.json",
        "/.env",
        "/System/Logs",
        "/Branding/..%2F..%2Fconfig.php",
    ] {
        let response = client
            .get(format!("{}{path}", fixture.url))
            .header("User-Agent", "Go-http-client/2.0")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    fixture.no_upstream_requests().await;
}

#[tokio::test]
async fn unauthenticated_public_routes_are_still_forwarded() {
    let fixture = super::support::Fixture::new().await;
    fixture.healthy().await;
    for upstream in &fixture.upstreams {
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(upstream)
            .await;
    }
    let response = reqwest::Client::new()
        .get(format!("{}/System/Ping", fixture.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(requests_to(&fixture, "/System/Ping").await, 1);
}

#[tokio::test]
async fn authenticated_unknown_routes_are_still_forwarded() {
    let fixture =
        super::support::Fixture::for_scenario(super::support::Scenario::NormalServers).await;
    fixture.healthy().await;
    for upstream in &fixture.upstreams {
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(upstream)
            .await;
    }
    let response = fixture
        .request(
            reqwest::Method::GET,
            "/SomePlugin/Configuration",
            &fixture.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(requests_to(&fixture, "/SomePlugin/Configuration").await, 1);
}

#[tokio::test]
async fn unknown_tokens_are_rejected_locally() {
    let fixture = super::support::Fixture::new().await;
    fixture.healthy().await;
    let client = reqwest::Client::new();
    for (path, header, value) in [
        ("/config.php", "X-Emby-Token", "not-a-real-token"),
        (
            "/SomePlugin/Configuration",
            "X-MediaBrowser-Token",
            "deadbeef",
        ),
        (
            "/System/Logs",
            "Authorization",
            r#"MediaBrowser Client="x", Device="y", DeviceId="z", Version="1", Token="bogus""#,
        ),
    ] {
        let response = client
            .get(format!("{}{path}", fixture.url))
            .header(header, value)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
    let response = client
        .get(format!("{}/config.php?api_key=bogus", fixture.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    fixture.no_upstream_requests().await;
}
