//! HTTP/WebSocket integration tests with real SQLite authorization and zero-upstream assertions.
use super::*;
use crate::{
    config::{AppConfig, MediaStreamingMode, MIGRATOR},
    handlers::quick_connect::QuickConnectStorage,
    media_storage_service::MediaStorageService,
    models::Authorization,
    server_storage::ServerStorageService,
    session_storage::SessionStorage,
    user_authorization_service::{Device, User, UserAuthorizationService},
    virtual_library_service::VirtualLibraryService,
    AppState, DataContext, ProxyProcessors,
};
use axum::{http::StatusCode, routing::get, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::{
    sync::mpsc,
    time::{timeout, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

struct Fixture {
    state: AppState,
    user: User,
    other: User,
    upstreams: Vec<wiremock::MockServer>,
    items: Vec<String>,
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn device(id: &str) -> Device {
    Device {
        client: "Test".into(),
        device: format!("Device {id}"),
        device_id: id.into(),
        version: "1".into(),
    }
}
fn auth(user: &User, id: &str) -> String {
    Authorization {
        client: "Test".into(),
        device: format!("Device {id}"),
        device_id: id.into(),
        version: "1".into(),
        token: Some(user.virtual_key.clone()),
    }
    .to_header_value()
}
impl Fixture {
    async fn new() -> Self {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let servers = ServerStorageService::new(pool.clone());
        let media = MediaStorageService::new(pool.clone());
        let data = DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(servers.clone()),
            media_storage: Arc::new(media.clone()),
            virtual_library_service: Arc::new(VirtualLibraryService::new(pool, servers, media)),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig {
                url_prefix: Some("proxy".into()),
                ..Default::default()
            })),
        };
        let state = AppState::new(
            reqwest::Client::new(),
            reqwest::Client::new(),
            data.clone(),
            ProxyProcessors::new(data),
            QuickConnectStorage::new(),
        );
        let user = state
            .user_authorization
            .create_user("alice", &"password".into())
            .await
            .unwrap();
        let other = state
            .user_authorization
            .create_user("bob", &"password".into())
            .await
            .unwrap();
        let mut upstreams = Vec::new();
        let mut items = Vec::new();
        for i in 0..2 {
            let upstream = wiremock::MockServer::start().await;
            let id = state
                .server_storage
                .add_server(
                    &format!("Server {i}"),
                    &upstream.uri(),
                    100,
                    MediaStreamingMode::Proxy,
                )
                .await
                .unwrap();
            let server = state
                .server_storage
                .get_server_by_id(id)
                .await
                .unwrap()
                .unwrap();
            state
                .user_authorization
                .add_server_mapping(&user.id, &server, "alice", &"password".into(), None)
                .await
                .unwrap();
            let authorization = Authorization {
                client: "Test".into(),
                device: "Device tv".into(),
                device_id: "tv".into(),
                version: "1".into(),
                token: None,
            };
            state
                .user_authorization
                .store_authorization_session(
                    &user.id,
                    &server,
                    &authorization,
                    format!("backend-token-{i}"),
                    format!("backend-user-{i}"),
                    None,
                )
                .await
                .unwrap();
            let mapping = state
                .media_storage
                .get_or_create_media_mapping(&uuid::Uuid::new_v4().simple().to_string(), &server)
                .await
                .unwrap();
            items.push(mapping.virtual_media_id);
            upstreams.push(upstream);
        }
        let routes = jellyswarrm_macros::lowercase_routes! {
            Router::new().merge(router()).route("/socket", get(websocket))
                .route("/SyncPlay/New", axum::routing::post(crate::handlers::syncplay::create_group))
                .route("/{*path}", axum::routing::any(crate::proxy_handler))
                .fallback(crate::proxy_handler)
        };
        let app = Router::new()
            .nest("/proxy", routes)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/proxy", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            state,
            user,
            other,
            upstreams,
            items,
            url,
            task,
        }
    }
    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        user: &User,
        device: &str,
    ) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .request(method, format!("{}{path}", self.url))
            .header("Authorization", auth(user, device))
    }
    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.request(reqwest::Method::POST, path, &self.user, "web")
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    async fn capabilities(&self) {
        let response = self.request(reqwest::Method::POST, "/Sessions/Capabilities/Full", &self.user, "tv").json(&json!({
            "SupportsMediaControl": true, "PlayableMediaTypes": ["Video", "Audio"], "SupportedCommands": ["DisplayMessage", "DisplayContent", "SetVolume", "SetRepeatMode"]
        })).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    async fn list(&self) -> Vec<Value> {
        self.request(
            reqwest::Method::GET,
            &format!("/Sessions?ControllableByUserId={}", self.user.id),
            &self.user,
            "web",
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
    }
    async fn no_upstream_requests(&self) {
        for upstream in &self.upstreams {
            assert!(upstream.received_requests().await.unwrap().is_empty());
        }
    }
    async fn socket(
        &self,
        device: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let url = format!(
            "{}/socket?api_key={}&deviceId={device}",
            self.url.replacen("http", "ws", 1),
            self.user.virtual_key
        );
        let (mut ws, _) = connect_async(url).await.unwrap();
        let first: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(first["MessageType"], "ForceKeepAlive");
        ws
    }
}

#[tokio::test]
async fn token_only_stream_with_device_query_keeps_upstream_access() {
    let f = Fixture::new().await;
    wiremock::Mock::given(wiremock::matchers::path("/System/Info/Public"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(json!({"ServerName":"Test"})),
        )
        .mount(&f.upstreams[0])
        .await;
    f.state.server_storage.check_servers_health().await;
    // Account for media traffic separately from the fixture health probes.
    for upstream in &f.upstreams {
        upstream.reset().await;
    }
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"fixture-media"))
        .mount(&f.upstreams[0])
        .await;
    let response = reqwest::Client::new()
        .get(format!("{}/Videos/{}/stream.mp4", f.url, f.items[0]))
        .query(&[
            ("ApiKey", f.user.virtual_key.as_str()),
            ("DeviceId", "tv"),
            ("Static", "true"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"fixture-media");
    assert!(f.upstreams[1].received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn token_only_browser_sockets_use_device_cookie_without_granting_authentication() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let f = Fixture::new().await;
    f.capabilities().await;
    let cookie = format!(
        "jellyswarrm_device={}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&["Test", "Device tv", "tv", "1"]).unwrap())
    );
    let socket_url = f.url.replacen("http", "ws", 1);
    let mut unauthorized = format!("{socket_url}/socket")
        .into_client_request()
        .unwrap();
    unauthorized
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());
    assert!(
        connect_async(unauthorized).await.is_err(),
        "cookie must not authenticate"
    );
    let mut request = format!("{socket_url}/socket?api_key={}", f.user.virtual_key)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());
    let (mut socket, _) = connect_async(request).await.unwrap();
    socket.next().await.unwrap().unwrap();
    let listed = f.list().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["DeviceId"], "tv");
    assert_eq!(listed[0]["SupportsRemoteControl"], true);

    // An explicit device ID must not be overwritten by the browser cookie.
    let mut request = format!(
        "{socket_url}/socket?api_key={}&deviceId=other",
        f.user.virtual_key
    )
    .into_client_request()
    .unwrap();
    request
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());
    let (mut other, _) = connect_async(request).await.unwrap();
    other.next().await.unwrap().unwrap();
    assert_eq!(f.list().await[0]["DeviceId"], "tv");
    f.no_upstream_requests().await;
}

#[tokio::test]
async fn real_socket_discovery_play_commands_and_subscriptions_stay_local() {
    let f = Fixture::new().await;
    f.capabilities().await;
    assert!(f.list().await.is_empty());
    let mut tv = f.socket("tv").await;
    let listed = f.list().await;
    assert_eq!(listed.len(), 1);
    let id = listed[0]["Id"].as_str().unwrap();
    assert_eq!(listed[0]["Client"], "Test");
    assert!(!serde_json::to_string(&listed)
        .unwrap()
        .contains(&f.user.virtual_key));
    assert_eq!(uuid::Uuid::parse_str(id).unwrap().simple().to_string(), id);
    let response = f
        .post(
            &format!(
                "/Sessions/{id}/Playing?playCommand=PlayNow&itemIds={}&startPositionTicks=123",
                f.items.join(",")
            ),
            Value::Null,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let msg: Value = serde_json::from_str(
        timeout(Duration::from_secs(2), tv.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(msg["MessageType"], "Play");
    assert_eq!(msg["Data"]["ItemIds"], json!(f.items));
    assert_eq!(msg["Data"]["ControllingUserId"], f.user.id);
    assert_eq!(msg["Data"]["StartPositionTicks"], 123);
    for command in ["Pause", "Unpause", "Stop"] {
        assert_eq!(
            f.post(
                &format!("/sessions/{id}/playing/{command}?controllingUserId=forged"),
                Value::Null
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
        let msg: Value =
            serde_json::from_str(tv.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(msg["MessageType"], "Playstate");
        assert_eq!(msg["Data"]["Command"], command);
        assert_eq!(msg["Data"]["ControllingUserId"], f.user.id);
    }
    let mut controller = f.socket("web").await;
    controller
        .send(Message::Text(
            json!({"MessageType":"SessionsStart", "Data":"0,250"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let update: Value = serde_json::from_str(
        timeout(Duration::from_secs(2), controller.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(update["MessageType"], "Sessions");
    assert!(update["Data"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["UserId"] == f.user.id));
    controller
        .send(Message::Text(
            json!({"MessageType":"SessionsStop"}).to_string().into(),
        ))
        .await
        .unwrap();
    tv.close(None).await.unwrap();
    // Poll until the server has processed the close; no fixed timing assumption.
    timeout(Duration::from_secs(2), async {
        loop {
            if f.list().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        f.post(&format!("/Sessions/{id}/Playing/Pause"), Value::Null)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    f.no_upstream_requests().await;
}

#[tokio::test]
async fn authorization_validation_and_unsupported_routes_never_fall_through() {
    let f = Fixture::new().await;
    let _tv = f.socket("tv").await;
    f.capabilities().await; // Socket-before-capabilities order.
    let listed = f.list().await;
    let id = listed[0]["Id"].as_str().unwrap();
    assert_eq!(
        f.request(
            reqwest::Method::POST,
            &format!("/Sessions/{id}/Playing/Pause"),
            &f.other,
            "other"
        )
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request(
            reqwest::Method::GET,
            &format!("/Sessions?ControllableByUserId={}", f.user.id),
            &f.other,
            "other"
        )
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.post(
            &format!("/Sessions/Capabilities/Full?id={id}"),
            json!({"SupportsMediaControl":true})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.post(
            &format!(
                "/Sessions/{id}/Playing?playCommand=PlayNow&itemIds={}",
                uuid::Uuid::new_v4()
            ),
            Value::Null
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.post(
            &format!(
                "/Sessions/{id}/Playing?playCommand=PlayNow&itemIds={}&startIndex=2",
                f.items[0]
            ),
            Value::Null
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.post(
            &format!("/Sessions/{id}/Playing/Seek?seekPositionTicks=-1"),
            Value::Null
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    for path in [
        format!("/Sessions/{id}/User/other"),
        "/Sessions/Unsupported".into(),
        "/SeSsIoNs/Unsupported".into(),
    ] {
        assert_eq!(
            f.post(&path, Value::Null).await.status(),
            StatusCode::NOT_IMPLEMENTED
        );
    }
    let key = f
        .state
        .user_authorization
        .create_api_key(&f.user.id, "read-only")
        .await
        .unwrap();
    let response = reqwest::Client::new()
        .post(format!("{}/Sessions/{id}/Playing/Pause", f.url))
        .header("X-Emby-Token", key.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    f.no_upstream_requests().await;
}

#[tokio::test]
async fn messages_playback_reports_and_syncplay_share_the_same_target() {
    let f = Fixture::new().await;
    f.capabilities().await;
    let mut tv = f.socket("tv").await;
    let listed = f.list().await;
    let id = listed[0]["Id"].as_str().unwrap();
    assert_eq!(
        f.post(
            &format!("/Sessions/{id}/Message"),
            json!({"Text":"Hello", "TimeoutMs":123})
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let msg: Value =
        serde_json::from_str(tv.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(msg["MessageType"], "GeneralCommand");
    assert_eq!(msg["Data"]["Arguments"]["TimeoutMs"], "123");
    assert_eq!(msg["Data"]["Name"], "DisplayMessage");
    f.state
        .client_sessions
        .report(
            id,
            false,
            &json!({"ItemId":f.items[0], "PositionTicks":12, "CanSeek":true, "IsPaused":true}),
        )
        .await;
    assert_eq!(f.list().await[0]["PlayState"]["PositionTicks"], 12);
    assert_eq!(
        f.post(
            &format!("/Sessions/{id}/Playing/Seek?seekPositionTicks=42"),
            Value::Null
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let msg: Value =
        serde_json::from_str(tv.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(msg["Data"]["SeekPositionTicks"], 42);
    // Commands do not invent observed playback state.
    assert_eq!(f.list().await[0]["PlayState"]["PositionTicks"], 12);
    let response = f
        .request(reqwest::Method::POST, "/SyncPlay/New", &f.user, "tv")
        .json(&json!({"GroupName":"Test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        f.post(&format!("/Sessions/{id}/Playing/Pause"), Value::Null)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.post(
            &format!("/Sessions/{id}/Command"),
            json!({"Name":"SetRepeatMode", "Arguments":{"RepeatMode":"RepeatAll"}})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.post(
            &format!("/Sessions/{id}/Message"),
            json!({"Text":"Still connected"})
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        f.request(reqwest::Method::POST, "/Sessions/Logout", &f.user, "tv")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert!(!f.state.syncplay.is_group_member(id).await);
    assert!(f.state.client_sessions.get(id).await.is_none());
    f.no_upstream_requests().await;
}

#[tokio::test]
async fn bounded_transport_replacement_and_closed_channels() {
    let hub = transport::ConnectionHub::default();
    let (tx, mut rx) = mpsc::channel(1);
    let old = hub.register("session".into(), tx);
    hub.send("session", "Play", &json!({})).unwrap();
    assert_eq!(
        hub.send("session", "Play", &json!({})),
        Err(StatusCode::SERVICE_UNAVAILABLE)
    );
    rx.recv().await.unwrap();
    let (tx, rx2) = mpsc::channel(1);
    let new = hub.register("session".into(), tx);
    assert!(!hub.unregister("session", old));
    assert!(hub.is_current("session", new));
    drop(rx2);
    assert_eq!(
        hub.send("session", "Play", &json!({})),
        Err(StatusCode::CONFLICT)
    );
    assert!(!hub.is_connected("session"));
}

#[tokio::test]
async fn identity_reconnect_revocation_and_authentication_dto_are_consistent() {
    let f = Fixture::new().await;
    let authorization = Authorization {
        client: "Test".into(),
        device: "Device tv".into(),
        device_id: "tv".into(),
        version: "1".into(),
        token: None,
    };
    let mut response: crate::models::AuthenticateResponse = serde_json::from_value(json!({
        "User":{"Id":"backend-user", "Name":"Alice", "ServerId":"backend", "Policy":{"IsAdministrator":false, "SyncPlayAccess":"None"}},
        "SessionInfo":{"Id":"backend-session", "UserId":"backend-user", "UserName":"Alice", "ServerId":"backend"},
        "AccessToken":"backend-token", "ServerId":"backend"
    })).unwrap();
    decorate_authentication(&f.state, &f.user, &authorization, &mut response).await;
    let id = response.session_info.extra["Id"]
        .as_str()
        .unwrap()
        .to_string();
    f.capabilities().await;
    let mut old = f.socket("tv").await;
    assert_eq!(f.list().await[0]["Id"], id);
    let mut new = f.socket("tv").await;
    let _ = old.close(None).await;
    assert_eq!(
        f.post(&format!("/Sessions/{id}/Playing/Pause"), Value::Null)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let msg: Value = serde_json::from_str(
        timeout(Duration::from_secs(2), new.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(msg["Data"]["Command"], "Pause");
    let other_device_id = f
        .state
        .client_sessions
        .ensure(&f.user, &f.user.virtual_key, Some(&device("tv-other")))
        .await;
    assert_ne!(id, other_device_id);
    // Invalid target credentials disappear before a command can be delivered.
    let revoked = f
        .state
        .client_sessions
        .ensure(&f.user, "revoked-token", Some(&device("revoked")))
        .await;
    f.state
        .client_sessions
        .capabilities(
            &revoked,
            models::Capabilities {
                supports_media_control: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::channel(1);
    f.state
        .client_sessions
        .transport
        .register(revoked.clone(), tx);
    assert_eq!(
        f.post(&format!("/Sessions/{revoked}/Playing/Pause"), Value::Null)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(f.state.client_sessions.get(&revoked).await.is_none());
    assert!(rx.recv().await.is_none());
    f.no_upstream_requests().await;
}

#[tokio::test]
async fn telemetry_still_reaches_its_media_backend_and_updates_local_state() {
    use wiremock::{
        matchers::{method, path},
        Mock, ResponseTemplate,
    };
    let f = Fixture::new().await;
    for server in &f.upstreams {
        Mock::given(method("GET"))
            .and(path("/System/Info/Public"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ServerName":"Test"})))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .mount(server)
            .await;
    }
    f.state.server_storage.check_servers_health().await;
    let id = f
        .state
        .client_sessions
        .ensure(&f.user, &f.user.virtual_key, Some(&device("tv")))
        .await;
    let item = &f.items[0];
    let playback_id = uuid::Uuid::new_v4().simple().to_string();
    let response = f.request(reqwest::Method::POST, "/Sessions/Playing", &f.user, "tv").json(&json!({"ItemId":item, "PlaySessionId":playback_id, "PositionTicks":12, "CanSeek":true})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let snapshot = f.state.client_sessions.get(&id).await.unwrap();
    assert_eq!(snapshot.now_playing["Id"], item.as_str());
    assert_eq!(snapshot.play_state["PositionTicks"], 12);
    let requests = f.upstreams[0].received_requests().await.unwrap();
    let report = requests.iter().find(|r| r.method == "POST").unwrap();
    let payload: Value = serde_json::from_slice(&report.body).unwrap();
    assert_ne!(payload["ItemId"], item.as_str());
    assert!(f.upstreams[1]
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method != "POST"));
    let response = f
        .request(
            reqwest::Method::POST,
            "/sessions/playing/stopped",
            &f.user,
            "tv",
        )
        .json(&json!({"ItemId":item, "PlaySessionId":playback_id, "PositionTicks":12}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(f
        .state
        .client_sessions
        .get(&id)
        .await
        .unwrap()
        .now_playing
        .is_null());
}

#[tokio::test]
async fn query_capabilities_filters_browse_and_admin_reset_are_local() {
    let f = Fixture::new().await;
    let mut tv = f.socket("tv").await;
    let response = f.request(reqwest::Method::POST, "/sessions/capabilities?playableMediaTypes=Video,Audio&supportedCommands=DisplayContent,SetVolume&supportsMediaControl=TRUE", &f.user, "tv").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let listed = f.list().await;
    let id = listed[0]["Id"].as_str().unwrap();
    let filter: Vec<Value> = f
        .request(
            reqwest::Method::GET,
            "/Sessions?DeviceId=tv&ActiveWithinSeconds=60",
            &f.user,
            "web",
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(filter.len(), 1);
    assert_eq!(
        filter[0]["Capabilities"]["SupportsPersistentIdentifier"],
        true
    );
    assert_eq!(
        f.post(
            &format!(
                "/Sessions/{id}/Viewing?ItemId={}&ItemName=Movie&ItemType=Movie",
                f.items[0]
            ),
            Value::Null
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let msg: Value =
        serde_json::from_str(tv.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(msg["Data"]["Name"], "DisplayContent");
    assert_eq!(msg["Data"]["Arguments"]["ItemId"], f.items[0]);
    assert_eq!(
        f.post(
            &format!("/Sessions/{id}/Command"),
            json!({"name":"SetVolume", "arguments":{"Volume":"30"}, "controllingUserId":"forged"})
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let msg: Value =
        serde_json::from_str(tv.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(msg["Data"]["ControllingUserId"], f.user.id);
    f.state
        .client_sessions
        .cache_media_response(
            &f.user.virtual_key,
            &json!({"Id":f.items[0], "Name":"Picture", "MediaType":"Photo"}),
        )
        .await;
    assert_eq!(
        f.post(
            &format!(
                "/Sessions/{id}/Playing?playCommand=PlayNow&itemIds={}",
                f.items[0]
            ),
            Value::Null
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    end_user_sessions(&f.state, &f.user.id).await;
    assert!(!f.state.client_sessions.transport.is_connected(id));
    assert!(f.state.client_sessions.get(id).await.is_none());
    f.no_upstream_requests().await;
}

#[tokio::test]
async fn encoded_session_paths_do_not_escape_local_handling() {
    let f = Fixture::new().await;
    for path in [
        "/Se%73sions/Capabilities/Full",
        "/Sessions%2Funknown/Playing",
        "//Sessions/Unsupported",
    ] {
        assert_eq!(
            f.post(path, json!({})).await.status(),
            StatusCode::NOT_IMPLEMENTED
        );
    }
    f.no_upstream_requests().await;
}

#[test]
fn discovery_filter_does_not_change_the_callers_device_identity() {
    let auth = crate::request_preprocessing::JellyfinAuthorization::Authorization(Authorization {
        client: "Test".into(),
        device: "Web".into(),
        device_id: "web".into(),
        version: "1".into(),
        token: Some("token".into()),
    });
    let request = reqwest::Request::new(
        reqwest::Method::GET,
        url::Url::parse("http://local/Sessions?DeviceId=tv").unwrap(),
    );
    assert_eq!(
        crate::request_preprocessing::request_device(&request, Some(&auth))
            .unwrap()
            .device_id,
        "web"
    );
    let request = reqwest::Request::new(
        reqwest::Method::GET,
        url::Url::parse("http://local/socket?deviceId=tv").unwrap(),
    );
    let token = crate::request_preprocessing::JellyfinAuthorization::XEmbyToken("token".into());
    assert_eq!(
        crate::request_preprocessing::request_device(&request, Some(&token))
            .unwrap()
            .device_id,
        "tv"
    );
}

#[tokio::test]
async fn distinct_clients_with_the_same_device_id_are_not_merged() {
    let f = Fixture::new().await;
    let first = device("shared");
    let second = Device {
        client: "Another client".into(),
        ..first.clone()
    };
    let a = f
        .state
        .client_sessions
        .ensure(&f.user, &f.user.virtual_key, Some(&first))
        .await;
    let b = f
        .state
        .client_sessions
        .ensure(&f.user, &f.user.virtual_key, Some(&second))
        .await;
    assert_ne!(a, b);
    let unknown = Device {
        client: "Unknown".into(),
        version: "Unknown".into(),
        ..first.clone()
    };
    let c = f
        .state
        .client_sessions
        .ensure(&f.user, &f.user.virtual_key, Some(&unknown))
        .await;
    assert_ne!(c, a);
    assert_ne!(c, b);
    assert_eq!(
        f.state
            .client_sessions
            .ensure(&f.user, &f.user.virtual_key, Some(&first))
            .await,
        a
    );
}
