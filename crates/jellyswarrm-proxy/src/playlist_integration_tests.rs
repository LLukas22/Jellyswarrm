//! HTTP coverage through the real proxy handler, preprocessing, SQLite mappings,
//! request/response processors and the production item-detail method router.
use super::*;
use models::Authorization;
use serde_json::{json, Value};
use user_authorization_service::User;
use wiremock::{
    matchers::{body_json, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

struct Fixture {
    state: AppState,
    upstreams: Vec<MockServer>,
    servers: Vec<Server>,
    caller: User,
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn authorization() -> Authorization {
    Authorization {
        client: "playlist-test".into(),
        device: "test".into(),
        device_id: "test".into(),
        version: "1".into(),
        token: None,
    }
}

impl Fixture {
    async fn new() -> Self {
        let pool = SqlitePoolOptions::new()
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
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        };
        let state = AppState::new(
            reqwest::Client::new(),
            reqwest::Client::new(),
            data.clone(),
            ProxyProcessors::new(data),
            QuickConnectStorage::new(),
        );
        let caller = state
            .user_authorization
            .create_user("caller", &"password".into())
            .await
            .unwrap();
        let mut upstreams = Vec::new();
        let mut servers = Vec::new();
        for index in 0..2 {
            let upstream = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/System/Info/Public"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"ServerName":"Test"})),
                )
                .mount(&upstream)
                .await;
            let id = state
                .server_storage
                .add_server(
                    &format!("Server {index}"),
                    &upstream.uri(),
                    100 - index,
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
            Self::map_user(&state, &caller, &server, &format!("caller-{index}")).await;
            servers.push(server);
            upstreams.push(upstream);
        }
        state.server_storage.check_servers_health().await;
        let app = Router::new()
            .route("/Items/{item_id}", item_detail_routes())
            .fallback(proxy_handler)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            state,
            upstreams,
            servers,
            caller,
            url,
            task,
        }
    }

    async fn map_user(state: &AppState, user: &User, server: &Server, upstream_id: &str) {
        state
            .user_authorization
            .add_server_mapping(
                &user.id,
                server,
                &user.original_username,
                &"password".into(),
                None,
            )
            .await
            .unwrap();
        state
            .user_authorization
            .store_authorization_session(
                &user.id,
                server,
                &authorization(),
                format!("token-{upstream_id}"),
                upstream_id.into(),
                None,
            )
            .await
            .unwrap();
    }

    async fn request(&self, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut request = self.state.reqwest_client.request(method, format!("{}{path}", self.url))
            .header("Authorization", format!("MediaBrowser Client=\"playlist-test\", Device=\"test\", DeviceId=\"test\", Version=\"1\", Token=\"{}\"", self.caller.virtual_key));
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.unwrap()
    }

    // Discover virtual track IDs through HTTP response translation, not pre-created mappings.
    async fn song(&self, index: usize) -> String {
        Mock::given(method("GET"))
            .and(path(format!("/TestSongs{index}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"Ids":[format!("song-{index}")]})),
            )
            .expect(1)
            .mount(&self.upstreams[index])
            .await;
        // This discovery endpoint has no IDs to route by. Select the server using
        // a mapped anchor; subsequent playlist calls use only discovered IDs.
        let anchor = self
            .state
            .media_storage
            .get_or_create_media_mapping("anchor", &self.servers[index])
            .await
            .unwrap();
        let response = self
            .request(
                Method::GET,
                &format!("/TestSongs{index}?ParentId={}", anchor.virtual_media_id),
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<Value>().await.unwrap()["Ids"][0]
            .as_str()
            .unwrap()
            .into()
    }

    async fn create(&self, song: &str) -> String {
        Mock::given(method("POST"))
            .and(path("/Playlists"))
            .and(body_json(
                json!({"Name":"Saved", "Ids":["song-0"], "UserId":"caller-0"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Id":"playlist"})))
            .expect(1)
            .mount(&self.upstreams[0])
            .await;
        let response = self
            .request(
                Method::POST,
                "/Playlists",
                Some(json!({"Name":"Saved", "Ids":[song], "UserId":self.caller.id})),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<Value>().await.unwrap()["Id"]
            .as_str()
            .unwrap()
            .into()
    }

    async fn mutation_count(&self) -> usize {
        let mut count = 0;
        for server in &self.upstreams {
            count += server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.method != "GET")
                .count();
        }
        count
    }
}

#[tokio::test]
async fn saved_playlist_http_lifecycle_preserves_duplicate_entries_and_deletes() {
    let f = Fixture::new().await;
    let song = f.song(0).await;
    let playlist = f.create(&song).await;
    Mock::given(method("POST"))
        .and(path("/Playlists/playlist/Items"))
        .and(query_param("Ids", "song-0"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    assert_eq!(
        f.request(
            Method::POST,
            &format!("/Playlists/{playlist}/Items?Ids={song}"),
            None
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );

    Mock::given(method("GET")).and(path("/Playlists/playlist/Items"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Type":"Playlist", "CanDelete":true,
            "Items":[{"Id":"song-0", "PlaylistItemId":"entry-1"}, {"Id":"song-0", "PlaylistItemId":"entry-2"}],
            "EntryIds":["entry-1", "entry-2"]})))
        .expect(1).mount(&f.upstreams[0]).await;
    let response = f
        .request(Method::GET, &format!("/Playlists/{playlist}/Items"), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let read: Value = response.json().await.unwrap();
    assert_eq!(read["CanDelete"], true);
    assert_eq!(read["Items"][0]["Id"], song);
    assert_eq!(read["Items"][1]["Id"], song);
    let first = read["Items"][0]["PlaylistItemId"].as_str().unwrap();
    let second = read["Items"][1]["PlaylistItemId"].as_str().unwrap();
    assert_ne!(first, second);
    assert_eq!(read["EntryIds"], json!([first, second]));

    Mock::given(method("POST"))
        .and(path("/Playlists/playlist/Items/entry-2/Move/0"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    assert_eq!(
        f.request(
            Method::POST,
            &format!("/Playlists/{playlist}/Items/{second}/Move/0"),
            None
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    Mock::given(method("DELETE"))
        .and(path("/Playlists/playlist/Items"))
        .and(query_param("EntryIds", "entry-1,entry-2"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    assert_eq!(
        f.request(
            Method::DELETE,
            &format!("/Playlists/{playlist}/Items?EntryIds={first},{second}"),
            None
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    Mock::given(method("DELETE"))
        .and(path("/Items/playlist"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    assert_eq!(
        f.request(Method::DELETE, &format!("/Items/{playlist}"), None)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(f.mutation_count().await, 5);
}

#[tokio::test]
async fn mixed_server_playlist_http_requests_are_rejected_before_forwarding() {
    let f = Fixture::new().await;
    let first = f.song(0).await;
    let second = f.song(1).await;
    let playlist = f.create(&first).await;
    for foreign in [&second, "ffffffff-ffff-ffff-ffff-fffffffffffe"] {
        for (path, body) in [
            ("/Playlists".into(), Some(json!({"Ids":[first, foreign]}))),
            (format!("/Playlists?Ids={first},{foreign}"), None),
            (format!("/Playlists/{playlist}/Items?Ids={foreign}"), None),
            (
                format!("/Playlists/{playlist}/Items"),
                Some(json!({"Ids":[foreign]})),
            ),
        ] {
            let before = f.mutation_count().await;
            let response = f.request(Method::POST, &path, body).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            assert!(response
                .text()
                .await
                .unwrap()
                .contains("mixed-server playlists are unsupported"));
            assert_eq!(
                f.mutation_count().await,
                before,
                "rejected request reached an upstream"
            );
        }
    }
}

#[tokio::test]
async fn sharing_http_requests_map_recipient_without_changing_caller_auth() {
    let f = Fixture::new().await;
    let song = f.song(0).await;
    let playlist = f.create(&song).await;
    let recipient = f
        .state
        .user_authorization
        .create_user("recipient", &"password".into())
        .await
        .unwrap();
    Fixture::map_user(&f.state, &recipient, &f.servers[0], "recipient-0").await;
    Mock::given(method("POST"))
        .and(path("/Playlists/playlist/Users/recipient-0"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    assert_eq!(
        f.request(
            Method::POST,
            &format!("/Playlists/{playlist}/Users/{}", recipient.id),
            None
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    Mock::given(method("POST"))
        .and(path("/Playlists/playlist/Users"))
        .and(body_json(json!({"UserId":"recipient-0", "CanEdit":true})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    assert_eq!(
        f.request(
            Method::POST,
            &format!("/Playlists/{playlist}/Users"),
            Some(json!({"UserId":recipient.id, "CanEdit":true}))
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let requests = f.upstreams[0].received_requests().await.unwrap();
    for request in requests.iter().filter(|r| r.url.path().contains("/Users")) {
        let auth = request
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(auth.contains("token-caller-0"));
        assert!(!auth.contains("token-recipient-0"));
    }
    let unmapped = f
        .state
        .user_authorization
        .create_user("unmapped", &"password".into())
        .await
        .unwrap();
    // A mapping on B must not be substituted into the A-owned playlist.
    Fixture::map_user(&f.state, &unmapped, &f.servers[1], "recipient-1").await;
    for id in [
        &unmapped.id,
        "ffffffff-ffff-ffff-ffff-fffffffffffd",
        "malformed-recipient",
    ] {
        for (path, body) in [
            (format!("/Playlists/{playlist}/Users/{id}"), None),
            (
                format!("/Playlists/{playlist}/Users"),
                Some(json!({"UserId":id})),
            ),
        ] {
            let before = f.mutation_count().await;
            let response = f.request(Method::POST, &path, body).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            assert!(response
                .text()
                .await
                .unwrap()
                .contains("Requested user has no mapping"));
            assert_eq!(f.mutation_count().await, before);
        }
    }
}
