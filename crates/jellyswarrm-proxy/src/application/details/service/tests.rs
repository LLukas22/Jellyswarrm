use std::sync::Arc;

use super::*;
use crate::{
    application::context::RequestContext,
    config::{AppConfig, MediaStreamingMode, MIGRATOR},
    handlers::quick_connect::QuickConnectStorage,
    media_storage_service::MediaStorageService,
    request_preprocessing::JellyfinAuthorization,
    server_storage::{Server, ServerStorageService},
    session_storage::SessionStorage,
    user_authorization_service::{AuthorizationSession, Device, UserAuthorizationService},
    virtual_library_service::{VirtualLibraryAccessScope, VirtualLibraryService},
    DataContext, ProxyProcessors,
};
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const LIBRARY: &str = "11111111111111111111111111111111";

struct Fixture {
    state: AppState,
    upstreams: Vec<MockServer>,
    sessions: Vec<(AuthorizationSession, Server)>,
    library_id: String,
}

impl Fixture {
    async fn new(statuses: [u16; 3]) -> Self {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let servers = ServerStorageService::new(pool.clone());
        let media = MediaStorageService::new(pool.clone());
        let context = DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(servers.clone()),
            media_storage: Arc::new(media.clone()),
            virtual_library_service: Arc::new(VirtualLibraryService::new(pool, servers, media)),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        };
        let state = AppState::new(
            reqwest::Client::new(),
            reqwest::Client::new(),
            context.clone(),
            ProxyProcessors::new(context),
            QuickConnectStorage::new(),
        );
        let mut upstreams = Vec::new();
        let mut sessions = Vec::new();
        for (index, status) in statuses.into_iter().enumerate() {
            let upstream = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/System/Info/Public"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "ServerName": "Backend"
                })))
                .expect(1)
                .mount(&upstream)
                .await;
            Mock::given(method("GET"))
                .and(path(format!("/Items/{LIBRARY}")))
                .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                    "Id": LIBRARY,
                    "Type": "CollectionFolder",
                    "CollectionType": "movies",
                    "Name": "Backend Movies",
                    "Overview": "third member detail"
                })))
                .expect(1)
                .mount(&upstream)
                .await;
            // Distinct priorities determine both the initial representative and
            // fallback order, independently of SQL row order and random ports.
            let id = state
                .server_storage
                .add_server(
                    &format!("Backend {index}"),
                    &upstream.uri(),
                    300 - index as i32 * 100,
                    MediaStreamingMode::Redirect,
                )
                .await
                .unwrap();
            let server = state
                .server_storage
                .get_server_by_id(id)
                .await
                .unwrap()
                .unwrap();
            let now = chrono::Utc::now();
            sessions.push((
                AuthorizationSession {
                    id: index as i64,
                    user_id: "viewer".into(),
                    mapping_id: index as i64,
                    server_url: upstream.uri(),
                    device: Device {
                        client: "test".into(),
                        device: "test".into(),
                        device_id: "device".into(),
                        version: "1".into(),
                    },
                    jellyfin_token: format!("backend-token-{index}"),
                    original_user_id: format!("backend-viewer-{index}"),
                    expires_at: None,
                    created_at: now,
                    updated_at: now,
                },
                server,
            ));
            upstreams.push(upstream);
        }
        state.server_storage.check_servers_health().await;
        let group = state
            .virtual_library_service
            .create_group("My Movies")
            .await
            .unwrap();
        // Deliberately do not insert members in representative/fallback order.
        for (_, server) in sessions.iter().rev() {
            state
                .virtual_library_service
                .add_member(&group.virtual_id, server.id, LIBRARY, "Movies", "movies")
                .await
                .unwrap();
        }
        Self {
            state,
            upstreams,
            sessions,
            library_id: group.virtual_id,
        }
    }

    fn operation(&self) -> OperationRequest {
        // Keep the configured virtual ID raw: production must choose and
        // translate the representative, then normalize each fallback member.
        let original_request = reqwest::Request::new(
            reqwest::Method::GET,
            format!("http://proxy/Items/{}", self.library_id)
                .parse()
                .unwrap(),
        );
        let context = RequestContext::new(
            &original_request,
            Some(JellyfinAuthorization::XEmbyToken("proxy-token".into())),
            None,
            Some(self.sessions.clone()),
            Some(VirtualLibraryAccessScope::new(
                "viewer",
                self.sessions.iter().map(|(_, server)| server.id),
            )),
        );
        OperationRequest {
            original_request,
            context,
            body_analysis: None,
        }
    }

    async fn assert_each_member_attempted_once_without_discovery(&self) {
        for (index, upstream) in self.upstreams.iter().enumerate() {
            let requests = upstream.received_requests().await.unwrap();
            let paths: Vec<_> = requests.iter().map(|request| request.url.path()).collect();
            assert_eq!(
                paths,
                ["/System/Info/Public", &format!("/Items/{LIBRARY}")],
                "backend {index}: exactly one health check and one detail attempt; no discovery"
            );
            upstream.verify().await;
        }
    }
}

#[tokio::test]
async fn configured_library_detail_continues_past_rate_limited_candidate() {
    let fixture = Fixture::new([404, 429, 200]).await;

    let detail = get_item_detail(&fixture.state, fixture.operation(), false)
        .await
        .unwrap();

    assert_eq!(detail["Id"], fixture.library_id);
    assert_eq!(detail["DisplayPreferencesId"], fixture.library_id);
    assert_eq!(detail["Name"], "My Movies");
    assert_eq!(detail["Overview"], "third member detail");
    fixture
        .assert_each_member_attempted_once_without_discovery()
        .await;
}

#[tokio::test]
async fn configured_library_detail_preserves_initial_not_found_when_all_candidates_fail() {
    let fixture = Fixture::new([404, 429, 503]).await;

    let error = get_item_detail(&fixture.state, fixture.operation(), false)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        ApplicationError::UpstreamStatus(StatusCode::NOT_FOUND)
    ));
    fixture
        .assert_each_member_attempted_once_without_discovery()
        .await;
}
