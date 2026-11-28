//! Moonfin's contract against the plugin API's in-memory `TestHost` and a mock
//! Seerr. Wiring into the production router is covered by the proxy's
//! `http_tests::moonfin`.
use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Method, Request, StatusCode},
    Router,
};
use jellyswarrm_plugin_api::{testing::TestHost, Caller, CredentialKind, Host, Plugin};
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::{
    matchers::{body_json, header, method, path, query_param},
    Mock, MockServer, Respond, ResponseTemplate,
};

use crate::PLUGIN;

const TOKEN: &str = "proxy-token";

fn alice() -> Caller {
    Caller {
        user_id: "alice-id".into(),
        user_name: "alice".into(),
        credential: CredentialKind::Login,
    }
}

/// Alice signs in with `TOKEN`.
fn host(settings: Option<Value>) -> Arc<TestHost> {
    let host = TestHost::new()
        .with_server_name("Swarm")
        .with_url_prefix("jellyfin")
        .with_user(TOKEN, alice());
    Arc::new(match settings {
        Some(settings) => host.with_settings(settings),
        None => host,
    })
}

/// Alice's stored Seerr session.
trait SessionStore {
    fn put(&self, caller: &Caller, key: &str, value: &str);
    fn session(&self) -> Option<Value>;
}

impl SessionStore for TestHost {
    fn put(&self, caller: &Caller, key: &str, value: &str) {
        self.set_secret(caller, key, Some(value));
    }
    fn session(&self) -> Option<Value> {
        self.secret(&alice(), "seerr_session")
            .map(|raw| serde_json::from_str(&raw).unwrap())
    }
}

struct Fixture {
    seerr: MockServer,
    host: Arc<TestHost>,
    router: Router,
}

impl Fixture {
    async fn new() -> Self {
        let seerr = MockServer::start().await;
        // A trailing slash is accepted and normalized away.
        let host = host(Some(json!({ "seerr_url": format!("{}/", seerr.uri()) })));
        let dyn_host: Arc<dyn Host> = host.clone();
        let router = PLUGIN.router(dyn_host).unwrap();
        Self {
            seerr,
            host,
            router,
        }
    }

    /// A session as `login` would store it.
    fn store_session(&self, cookie: &str, validated_ago_ms: i64) -> Value {
        let now = crate::seerr::now_ms();
        let session = json!({
            "seerr_url": self.seerr.uri(),
            "cookie_name": "connect.sid",
            "cookie_value": cookie,
            "seerr_user_id": 3,
            "display_name": "Alice",
            "avatar": null,
            "permissions": 2,
            "created_at": now,
            "last_validated": now - validated_ago_ms,
        });
        self.host
            .put(&alice(), "seerr_session", &session.to_string());
        session
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("MediaBrowser Token=\"{TOKEN}\""));
        let request = match body {
            Some(body) => builder
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => builder.body(Body::empty()),
        }
        .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let body = to_bytes(body, usize::MAX).await.unwrap();
        (
            parts.status,
            parts.headers,
            serde_json::from_slice(&body).unwrap_or(Value::Null),
        )
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        let (status, _, body) = self.send(Method::GET, uri, None).await;
        (status, body)
    }
}

#[test]
fn moonfin_mounts_only_with_a_usable_seerr_url() {
    for settings in [
        None,
        Some(json!({})),
        Some(json!({"seerr_url": "  "})),
        Some(json!({"seerr_url": "seerr:5055"})),
        Some(json!({"seerr_url": "ftp://seerr"})),
        // Misspelled keys and wrong types are reported, not ignored.
        Some(json!({"serr_url": "http://seerr:5055"})),
        Some(json!({"seerr_url": 5055})),
    ] {
        assert!(
            PLUGIN.router(host(settings.clone())).is_none(),
            "{settings:?}"
        );
    }
    assert!(PLUGIN
        .router(host(Some(json!({"seerr_url": "http://seerr:5055"}))))
        .is_some());
}

#[tokio::test]
async fn ping_and_config_report_seerr() {
    let f = Fixture::new().await;
    let (status, ping) = f.get("/Moonfin/Ping").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ping["installed"], true);
    assert_eq!(ping["serverName"], "Swarm");
    assert_eq!(ping["seerrEnabled"], true);
    assert_eq!(ping["seerrUrl"], f.seerr.uri());
    // Moonfin treats an explicit `false` here as "plugin unavailable".
    assert!(ping.get("settingsSyncEnabled").is_none(), "{ping}");

    for uri in [
        "/Moonfin/Seerr/Config",
        "/Moonfin/Jellyseerr/Config",
        "/moonfin/seerr/config",
    ] {
        let (status, config) = f.get(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(
            config,
            json!({
                "enabled": true,
                "url": f.seerr.uri(),
                "displayName": "Seerr",
                "variant": "seerr",
                "userEnabled": true,
            })
        );
    }
}

#[tokio::test]
async fn moonfin_requires_sign_in_with_a_reason() {
    let f = Fixture::new().await;
    let request = Request::get("/Moonfin/Seerr/Status")
        .body(Body::empty())
        .unwrap();
    let response = f.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    // Moonfin only shows a reason when the body carries `error`.
    assert_eq!(body["error"], "User not authenticated");
}

#[tokio::test]
async fn discovery_is_anonymous_and_reports_the_public_address() {
    let f = Fixture::new().await;
    let request = Request::get("/moonfin/discovery/discover")
        .header("host", "internal:3000")
        .header("x-forwarded-host", "media.example")
        .header("x-forwarded-proto", "https")
        .body(Body::empty())
        .unwrap();
    let response = f.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["cache-control"]
        .to_str()
        .unwrap()
        .contains("no-store"));
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        body,
        json!([{"id": "jellyfin-local", "name": "Swarm", "address": "https://media.example/jellyfin", "type": "Jellyfin"}])
    );
}

#[tokio::test]
async fn login_stores_one_session_with_csrf_and_returns_the_seerr_user() {
    let f = Fixture::new().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .respond_with(
            ResponseTemplate::new(403)
                .append_header("Set-Cookie", "_csrf=secret; Path=/; HttpOnly")
                .append_header("Set-Cookie", "XSRF-TOKEN=tok%2Ben; Path=/"),
        )
        .mount(&f.seerr)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/jellyfin"))
        .and(header("X-XSRF-TOKEN", "tok+en"))
        .and(header("Cookie", "_csrf=secret; XSRF-TOKEN=tok%2Ben"))
        .and(body_json(json!({ "username": "alice", "password": "pw" })))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("Set-Cookie", "connect.sid=s%3Aabc; Path=/; HttpOnly")
                .set_body_json(json!({
                    "id": 5,
                    "displayName": "Alice A",
                    "avatar": "/avatar.png",
                    "permissions": 32,
                })),
        )
        .expect(1)
        .mount(&f.seerr)
        .await;

    let (status, _, body) = f
        .send(
            Method::POST,
            "/Moonfin/Seerr/Login",
            Some(json!({ "Username": "alice", "Password": "pw" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "success": true,
            "seerrUserId": 5,
            "jellyseerrUserId": 5,
            "displayName": "Alice A",
            "avatar": "/avatar.png",
            "permissions": 32,
        })
    );
    let session = f.host.session().unwrap();
    assert_eq!(session["cookie_name"], "connect.sid");
    assert_eq!(session["cookie_value"], "s%3Aabc");
    assert_eq!(session["seerr_url"], f.seerr.uri());
}

#[tokio::test]
async fn failed_login_keeps_any_earlier_session() {
    let f = Fixture::new().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/jellyfin"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&f.seerr)
        .await;

    let login = json!({ "username": "alice", "password": "wrong" });
    let (status, _, body) = f
        .send(Method::POST, "/Moonfin/Seerr/Login", Some(login.clone()))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["success"], false);
    assert!(f.host.session().is_none());

    let earlier = f.store_session("s%3Aearlier", 0);
    f.send(Method::POST, "/Moonfin/Seerr/Login", Some(login))
        .await;
    assert_eq!(f.host.session(), Some(earlier));
}

#[tokio::test]
async fn invalid_login_responses_preserve_the_previous_session() {
    for response in [
        ResponseTemplate::new(200).set_body_raw("<html>Sign in</html>", "text/html"),
        ResponseTemplate::new(200).set_body_json(json!({ "displayName": "Alice" })),
        ResponseTemplate::new(200).set_body_json(json!({ "id": 0 })),
    ] {
        let f = Fixture::new().await;
        let previous = f.store_session("working", 0);
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/jellyfin"))
            .respond_with(response.append_header("Set-Cookie", "proxy.sid=unrelated; Path=/"))
            .expect(1)
            .mount(&f.seerr)
            .await;

        let (status, _, body) = f
            .send(
                Method::POST,
                "/Moonfin/Seerr/Login",
                Some(json!({ "username": "alice", "password": "pw" })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["success"], false);
        assert_eq!(f.host.session(), Some(previous));
    }
}

#[tokio::test]
async fn quick_connect_login_is_refused_without_contacting_seerr() {
    let f = Fixture::new().await;
    let (status, _, body) = f
        .send(
            Method::POST,
            "/Moonfin/Seerr/Login",
            Some(json!({ "authType": "quickconnect" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["errorCode"], "sso_unsupported");
    assert!(f.seerr.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn api_proxy_forwards_path_query_and_cookie() {
    let f = Fixture::new().await;
    f.store_session("s%3Aabc", 0);
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("query", "the matrix"))
        .and(query_param("page", "1"))
        .and(header("Cookie", "connect.sid=s%3Aabc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [] })))
        .expect(1)
        .mount(&f.seerr)
        .await;

    let (status, body) = f
        .get("/Moonfin/Seerr/Api/search?query=the%20matrix&page=1")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "results": [] }));
}

#[tokio::test]
async fn nested_api_preserves_encoded_paths_and_legacy_alias() {
    let f = Fixture::new().await;
    f.store_session("session", 0);
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&f.seerr)
        .await;
    let (status, _) = f.get("/moonfin/jellyseerr/api/search/a%2Fb%20c").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        f.seerr.received_requests().await.unwrap()[0].url.path(),
        "/api/v1/search/a%2Fb%20c"
    );
}

#[tokio::test]
async fn api_proxy_forwards_post_body_and_status() {
    let f = Fixture::new().await;
    f.store_session("s%3Aabc", 0);
    Mock::given(method("POST"))
        .and(path("/api/v1/request"))
        .and(body_json(json!({ "mediaType": "movie", "mediaId": 603 })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&f.seerr)
        .await;

    let (status, _, body) = f
        .send(
            Method::POST,
            "/Moonfin/Jellyseerr/Api/request",
            Some(json!({ "mediaType": "movie", "mediaId": 603 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["id"], 1);
}

#[tokio::test]
async fn api_proxy_without_session_is_unauthorized() {
    let f = Fixture::new().await;
    let (status, body) = f.get("/Moonfin/Seerr/Api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "NO_SESSION");
}

#[tokio::test]
async fn a_session_for_another_seerr_is_never_sent() {
    let f = Fixture::new().await;
    let mut session = f.store_session("s%3Aold", 0);
    session["seerr_url"] = json!("http://previous-seerr.example");
    f.host.put(&alice(), "seerr_session", &session.to_string());

    let (status, body) = f.get("/Moonfin/Seerr/Api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "NO_SESSION");
    let (_, status) = f.get("/Moonfin/Seerr/Status").await;
    assert_eq!(status["authenticated"], false);
    assert!(f.seerr.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn permission_denial_keeps_a_live_session() {
    let f = Fixture::new().await;
    let session = f.store_session("s%3Aabc", 0);
    Mock::given(method("DELETE"))
        .and(path("/api/v1/request/9"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&f.seerr)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .and(header("Cookie", "connect.sid=s%3Aabc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 3 })))
        .mount(&f.seerr)
        .await;

    let (status, _, _) = f
        .send(Method::DELETE, "/Moonfin/Seerr/Api/request/9", None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(f.host.session(), Some(session));
}

#[tokio::test]
async fn an_expired_session_is_reported_and_cleared() {
    let f = Fixture::new().await;
    f.store_session("s%3Aold", 0);
    Mock::given(method("GET"))
        .and(path("/api/v1/request"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&f.seerr)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&f.seerr)
        .await;

    let (status, body) = f.get("/Moonfin/Seerr/Api/request").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "SESSION_EXPIRED");
    assert!(f.host.session().is_none());
}

#[tokio::test]
async fn a_rotated_cookie_replaces_the_stored_one() {
    let f = Fixture::new().await;
    f.store_session("s%3Aold", 0);
    Mock::given(method("GET"))
        .and(path("/api/v1/request"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("Set-Cookie", "connect.sid=s%3Anew; Path=/")
                .set_body_json(json!({ "results": [] })),
        )
        .mount(&f.seerr)
        .await;

    let (status, _) = f.get("/Moonfin/Seerr/Api/request").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.host.session().unwrap()["cookie_value"], "s%3Anew");
}

#[tokio::test]
async fn stale_dead_session_status_is_reported_and_cleared() {
    let f = Fixture::new().await;
    f.store_session("s%3Aold", 10 * 60 * 1000);
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&f.seerr)
        .await;

    let (_, status) = f.get("/Moonfin/Seerr/Status").await;
    assert_eq!(status["enabled"], true);
    assert_eq!(status["authenticated"], false);
    assert!(f.host.session().is_none());
}

#[tokio::test]
async fn fresh_session_status_skips_seerr() {
    let f = Fixture::new().await;
    f.store_session("s%3Aabc", 0);
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&f.seerr)
        .await;

    let (_, status) = f.get("/Moonfin/Seerr/Status").await;
    assert_eq!(status["authenticated"], true);
    assert_eq!(status["seerrUserId"], 3);
    assert_eq!(status["displayName"], "Alice");
}

#[tokio::test]
async fn validation_refreshes_the_seerr_user() {
    let f = Fixture::new().await;
    f.store_session("s%3Aabc", 0);
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "id": 3, "permissions": 64 })),
        )
        .mount(&f.seerr)
        .await;

    let (_, validate) = f.get("/Moonfin/Seerr/Validate").await;
    assert_eq!(validate["valid"], true);
    assert_eq!(f.host.session().unwrap()["permissions"], 64);
}

/// Answers `auth/me` like Seerr, but first performs a new login for the same
/// user, as a second Moonfin client would while the check is in flight.
struct LoginDuringCheck {
    host: Arc<TestHost>,
    newer: String,
    status: u16,
}

impl Respond for LoginDuringCheck {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        self.host.put(&alice(), "seerr_session", &self.newer);
        ResponseTemplate::new(self.status).set_body_json(json!({ "id": 3, "permissions": 1 }))
    }
}

#[tokio::test]
async fn a_slow_validation_never_overwrites_or_clears_a_newer_login() {
    for status in [200, 403] {
        let f = Fixture::new().await;
        f.store_session("s%3Aold", 10 * 60 * 1000);
        let mut newer = f.store_session("s%3Anewer", 0);
        newer["display_name"] = json!("Newer login");
        f.store_session("s%3Aold", 10 * 60 * 1000);
        Mock::given(method("GET"))
            .and(path("/api/v1/auth/me"))
            .respond_with(LoginDuringCheck {
                host: f.host.clone(),
                newer: newer.to_string(),
                status,
            })
            .mount(&f.seerr)
            .await;

        let (_, report) = f.get("/Moonfin/Seerr/Status").await;
        assert_eq!(
            f.host.session(),
            Some(newer.clone()),
            "Seerr answered {status}"
        );
        assert_eq!(report["authenticated"], true);
        assert_eq!(report["displayName"], "Newer login");
    }
}

#[tokio::test]
async fn logout_signs_out_of_seerr_and_clears_the_session() {
    let f = Fixture::new().await;
    f.store_session("s%3Aabc", 0);
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/logout"))
        .and(header("Cookie", "connect.sid=s%3Aabc"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&f.seerr)
        .await;

    let (status, _, body) = f.send(Method::DELETE, "/Moonfin/Seerr/Logout", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);
    assert!(f.host.session().is_none());
}

#[tokio::test]
async fn validation_during_upstream_logout_sees_the_user_signed_out() {
    let f = Fixture::new().await;
    f.store_session("old", 0);
    let started = Arc::new(tokio::sync::Notify::new());
    let signal = started.clone();
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/logout"))
        .respond_with(move |_: &wiremock::Request| {
            signal.notify_one();
            ResponseTemplate::new(200)
        })
        .expect(1)
        .mount(&f.seerr)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .and(header("Cookie", "connect.sid=old"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 3 })))
        .expect(0)
        .mount(&f.seerr)
        .await;

    let logout = f.send(Method::DELETE, "/Moonfin/Seerr/Logout", None);
    tokio::pin!(logout);
    // Pause the local handler at its upstream request while validation runs.
    tokio::select! {
        biased;
        () = started.notified() => {},
        _ = &mut logout => panic!("logout finished before the upstream request"),
    }
    let (status, validation) = f.get("/Moonfin/Seerr/Validate").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(validation["valid"], false);
    assert_eq!(logout.await.0, StatusCode::OK);
    assert!(f.host.session().is_none());
}

#[tokio::test]
async fn logout_preserves_a_concurrent_login() {
    let f = Fixture::new().await;
    let newer = f.store_session("new", 0);
    f.store_session("old", 0);
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/logout"))
        .respond_with(LoginDuringCheck {
            host: f.host.clone(),
            newer: newer.to_string(),
            status: 200,
        })
        .mount(&f.seerr)
        .await;
    assert_eq!(
        f.send(Method::DELETE, "/Moonfin/Seerr/Logout", None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(f.host.session(), Some(newer));
}

#[tokio::test]
async fn inconclusive_validation_preserves_cached_status() {
    for status in [200, 503] {
        let f = Fixture::new().await;
        let previous = f.store_session("old", 600_000);
        Mock::given(method("GET"))
            .and(path("/api/v1/auth/me"))
            .respond_with(ResponseTemplate::new(status).set_body_string("unavailable"))
            .mount(&f.seerr)
            .await;
        assert_eq!(
            f.get("/Moonfin/Seerr/Validate").await.0,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            f.get("/Moonfin/Seerr/Status").await.1["authenticated"],
            true
        );
        assert_eq!(f.host.session(), Some(previous));
    }
}

#[tokio::test]
async fn jellyswarrm_credentials_never_reach_seerr() {
    let f = Fixture::new().await;
    f.store_session("s%3Aabc", 0);
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&f.seerr)
        .await;

    let (status, _) = f
        .get("/Moonfin/Seerr/Api/search?api_key=a&APIKEY=b&ApiKey=c&query=x")
        .await;
    assert_eq!(status, StatusCode::OK);
    let requests = f.seerr.received_requests().await.unwrap();
    assert_eq!(requests[0].url.query(), Some("query=x"));
    assert!(!requests[0].headers.contains_key("authorization"));
}
