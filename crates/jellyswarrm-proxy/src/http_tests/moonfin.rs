//! The Moonfin plugin on the production router: served by the proxy when
//! configured, default routing otherwise and for paths it doesn't implement.
use super::*;
use crate::config::AppConfig;

async fn configured() -> Fixture {
    with_seerr("http://seerr.example:5055").await
}

async fn with_seerr(seerr_url: &str) -> Fixture {
    let mut config = AppConfig::default();
    let mut moonfin = toml::Table::new();
    moonfin.insert("seerr_url".into(), seerr_url.into());
    config.plugins.insert("moonfin".into(), moonfin);
    let f = Fixture::with_config(config).await;
    f.healthy().await;
    f
}

async fn forwarded_paths(f: &Fixture) -> Vec<String> {
    let mut paths = Vec::new();
    for upstream in &f.upstreams {
        for request in upstream.received_requests().await.unwrap() {
            paths.push(request.url.path().to_owned());
        }
    }
    paths
}

#[tokio::test]
async fn configured_moonfin_answers_ping_and_discovery_without_backends() {
    let f = configured().await;

    let ping = f
        .request(reqwest::Method::GET, "/Moonfin/Ping", &f.user, "web")
        .send()
        .await
        .unwrap();
    assert_eq!(ping.status(), StatusCode::OK);
    let ping: Value = ping.json().await.unwrap();
    assert_eq!(ping["installed"], true);
    assert_eq!(ping["serverName"], AppConfig::default().server_name);
    assert_eq!(ping["seerrEnabled"], true);

    // Moonfin's Seerr client sends only the token, without device fields.
    let bare = reqwest::Client::new()
        .get(format!("{}/moonfin/ping", f.url))
        .header(
            "Authorization",
            format!("MediaBrowser Token=\"{}\"", f.user.fixture_token),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), StatusCode::OK);

    let anonymous = reqwest::Client::new()
        .get(format!("{}/Moonfin/Ping", f.url))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let discovery: Value = reqwest::Client::new()
        .get(format!("{}/Moonfin/Discovery", f.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(discovery[0]["address"], f.url, "includes the URL prefix");

    f.no_upstream_requests().await;
}

#[tokio::test]
async fn unimplemented_moonfin_paths_take_default_routing() {
    let f = configured().await;
    let response = f
        .request(reqwest::Method::GET, "/Moonfin/Settings", &f.user, "web")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(forwarded_paths(&f).await, ["/Moonfin/Settings"]);
}

#[tokio::test]
async fn unconfigured_moonfin_takes_default_routing() {
    let f = Fixture::new().await;
    f.healthy().await;
    let response = f
        .request(reqwest::Method::GET, "/Moonfin/Ping", &f.user, "web")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(forwarded_paths(&f).await, ["/Moonfin/Ping"]);
}

/// Moonfin's Seerr client signs in with the token alone.
fn token_only(f: &Fixture) -> String {
    format!("MediaBrowser Token=\"{}\"", f.user.fixture_token)
}

#[tokio::test]
async fn seerr_login_and_requests_use_an_encrypted_per_user_session() {
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, ResponseTemplate,
    };
    let seerr = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/jellyfin"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("Set-Cookie", "connect.sid=s%3Asecret-cookie; Path=/")
                .set_body_json(json!({ "id": 5 })),
        )
        .mount(&seerr)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/request"))
        .and(header("Cookie", "connect.sid=s%3Asecret-cookie"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [1] })))
        .expect(1)
        .mount(&seerr)
        .await;
    let f = with_seerr(&seerr.uri()).await;

    let login = reqwest::Client::new()
        .post(format!("{}/Moonfin/Seerr/Login", f.url))
        .header("Authorization", token_only(&f))
        .json(&json!({ "username": "alice", "password": "pw" }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);

    // Even after this user signs in, their application key cannot borrow the
    // Seerr session on GET. Ping and Config only read proxy configuration.
    let (key, _) = f
        .state
        .user_authorization
        .create_api_key(&f.user, "Moonfin")
        .await
        .unwrap();
    let before = seerr.received_requests().await.unwrap().len();
    for (route, expected) in [
        ("/Moonfin/Seerr/Api/request", StatusCode::FORBIDDEN),
        ("/Moonfin/Jellyseerr/Api/request", StatusCode::FORBIDDEN),
        ("/Moonfin/Seerr/Status", StatusCode::FORBIDDEN),
        ("/Moonfin/Seerr/Validate", StatusCode::FORBIDDEN),
        ("/Moonfin/Seerr/Config", StatusCode::OK),
        ("/Moonfin/Ping", StatusCode::OK),
    ] {
        let response = reqwest::Client::new()
            .get(format!("{}{route}", f.url))
            .header("X-Emby-Token", &key.access_token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{route}");
    }
    assert_eq!(seerr.received_requests().await.unwrap().len(), before);

    let stored: Vec<(String, String)> =
        sqlx::query_as("SELECT plugin_id, value FROM plugin_user_secrets WHERE user_id = ?")
            .bind(f.user.id.as_str())
            .fetch_all(&f.pool)
            .await
            .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].0, "moonfin");
    assert!(!stored[0].1.contains("secret-cookie"), "stored encrypted");

    let requests = reqwest::Client::new()
        .get(format!("{}/Moonfin/Seerr/Api/request", f.url))
        .query(&[
            ("api_key", f.user.fixture_token.as_str()),
            ("ApiKey", "duplicate"),
            ("query", "a b"),
            ("page", "2"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(requests.status(), StatusCode::OK);
    assert_eq!(requests.json::<Value>().await.unwrap()["results"][0], 1);

    let upstream = seerr.received_requests().await.unwrap();
    let fetched = upstream
        .iter()
        .find(|r| r.url.path() == "/api/v1/request")
        .unwrap();
    assert_eq!(fetched.url.query(), Some("query=a+b&page=2"));

    // Another user has no Seerr session of their own.
    let other = f
        .request(
            reqwest::Method::GET,
            "/Moonfin/Seerr/Api/request",
            &f.other,
            "web",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::UNAUTHORIZED);
    f.no_upstream_requests().await;
}

#[tokio::test]
async fn read_only_api_keys_cannot_change_anything_in_seerr() {
    let seerr = wiremock::MockServer::start().await;
    let f = with_seerr(&seerr.uri()).await;
    let (key, _) = f
        .state
        .user_authorization
        .create_api_key(&f.user, "Moonfin")
        .await
        .unwrap();
    for path in ["/Moonfin/Seerr/Login", "/Moonfin/Seerr/Api/request"] {
        let response = reqwest::Client::new()
            .post(format!("{}{path}", f.url))
            .header("X-Emby-Token", &key.access_token)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    assert!(seerr.received_requests().await.unwrap().is_empty());
    f.no_upstream_requests().await;
}
