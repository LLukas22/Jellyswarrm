//! Real SQLite identity/session state, real production media routes, and two
//! independently observable backends. Unmatched upstream requests return 404.
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
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_tungstenite::connect_async;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scenario {
    /// Connected servers retain separate library and item identities.
    NormalServers,
    /// Automatic library grouping plus selectable provider-matched versions.
    MergedLibraries,
}

impl Scenario {
    pub(super) fn merged(self) -> bool {
        self == Self::MergedLibraries
    }
}

pub(crate) struct Fixture {
    pub(crate) state: AppState,
    pub(crate) user: User,
    pub(crate) other: User,
    pub(crate) upstreams: Vec<wiremock::MockServer>,
    pub(crate) items: Vec<String>,
    pub(crate) url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub(crate) fn device(id: &str) -> Device {
    Device {
        client: "Test".into(),
        device: format!("Device {id}"),
        device_id: id.into(),
        version: "1".into(),
    }
}
pub(crate) fn auth(user: &User, id: &str) -> String {
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
    pub(crate) async fn new() -> Self {
        // Existing focused regressions may override individual settings themselves.
        Self::with_config(AppConfig::default()).await
    }

    pub(super) async fn for_scenario(scenario: Scenario) -> Self {
        Self::with_config(AppConfig {
            merge_libraries: scenario.merged(),
            deduplicate_media: scenario.merged(),
            ..Default::default()
        })
        .await
    }

    async fn with_config(config: AppConfig) -> Self {
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
                ..config
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
            Router::new().merge(crate::sessions::router())
                .merge(crate::client_api_routes::router())
                .route("/socket", get(crate::sessions::websocket))
                .route("/SyncPlay/New", axum::routing::post(crate::handlers::syncplay::create_group))
                .route("/{*path}", axum::routing::any(crate::proxy_handler))
                .fallback(crate::proxy_handler)
        };
        let routes = routes.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::enforce_read_only_api_keys,
        ));
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
    pub(crate) fn request(
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
    pub(crate) async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.request(reqwest::Method::POST, path, &self.user, "web")
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    pub(crate) async fn capabilities(&self) {
        let response = self.request(reqwest::Method::POST, "/Sessions/Capabilities/Full", &self.user, "tv").json(&json!({"SupportsMediaControl":true,"PlayableMediaTypes":["Video","Audio"],"SupportedCommands":["DisplayMessage","DisplayContent","SetVolume","SetRepeatMode"]})).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    pub(crate) async fn list(&self) -> Vec<Value> {
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
    pub(crate) async fn no_upstream_requests(&self) {
        for upstream in &self.upstreams {
            assert!(upstream.received_requests().await.unwrap().is_empty());
        }
    }
    pub(crate) async fn socket(
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

    pub(super) async fn healthy(&self) {
        for upstream in &self.upstreams {
            wiremock::Mock::given(wiremock::matchers::path("/System/Info/Public"))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .set_body_json(json!({"ServerName":"Backend"})),
                )
                .mount(upstream)
                .await;
        }
        self.state.server_storage.check_servers_health().await;
        for upstream in &self.upstreams {
            upstream.reset().await;
        }
    }

    pub(super) async fn original(&self, index: usize) -> String {
        self.state
            .media_storage
            .get_media_mapping_by_virtual(&self.items[index])
            .await
            .unwrap()
            .unwrap()
            .original_media_id
    }
}
