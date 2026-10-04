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
    pool: sqlx::SqlitePool,
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
            virtual_library_service: Arc::new(VirtualLibraryService::new(
                pool.clone(),
                servers,
                media,
            )),
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
            pool,
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
        self.media(index, &format!("song-{index}"), "Audio").await
    }

    async fn media(&self, index: usize, original_id: &str, kind: &str) -> String {
        Mock::given(method("GET"))
            .and(path(format!("/TestSongs{index}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Ids":[original_id], "Items":[{"Id":original_id, "Type":kind}]
            })))
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
async fn collection_membership_writes_translate_owner_and_movie_ids() {
    let f = Fixture::new().await;
    let movie = f.media(1, "movie", "Movie").await;
    let collection = f
        .state
        .media_storage
        .get_or_create_media_mapping("collection", &f.servers[1])
        .await
        .unwrap();
    for method in [Method::POST, Method::DELETE] {
        Mock::given(wiremock::matchers::method(method.as_str()))
            .and(path("/Collections/collection/Items"))
            .and(query_param("Ids", "movie"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&f.upstreams[1])
            .await;
        assert_eq!(
            f.request(
                method,
                &format!(
                    "/Collections/{}/Items?Ids={movie}",
                    collection.virtual_media_id
                ),
                None
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
    }
    assert!(f.upstreams[0]
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| request.method == "GET"));
}

#[tokio::test]
async fn collection_writes_reject_foreign_unknown_and_malformed_ids_before_forwarding() {
    let f = Fixture::new().await;
    let local = f.media(0, "movie", "Movie").await;
    let foreign = f.media(1, "series", "Series").await;
    let collection = f
        .state
        .media_storage
        .get_or_create_media_mapping("collection", &f.servers[0])
        .await
        .unwrap();
    for invalid in [
        &foreign,
        "ffffffff-ffff-ffff-ffff-fffffffffffe",
        "malformed",
    ] {
        for (url, body) in [
            (format!("/Collections?Ids={local},{invalid}"), None),
            (format!("/Collections?ids={local}&ids={invalid}"), None),
            ("/Collections".into(), Some(json!({"Ids":[local, invalid]}))),
            (
                format!(
                    "/Collections/{}/Items?Ids={invalid}",
                    collection.virtual_media_id
                ),
                None,
            ),
        ] {
            let before = f.mutation_count().await;
            let response = f.request(Method::POST, &url, body).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{url}");
            assert!(response
                .text()
                .await
                .unwrap()
                .contains("mixed-server collections are unsupported"));
            assert_eq!(f.mutation_count().await, before);
        }
    }
    let response = f
        .request(
            Method::POST,
            &format!("/Collections/malformed/Items?Ids={local}"),
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(f.mutation_count().await, 0);
}

#[tokio::test]
async fn playlist_detail_and_sharing_reads_translate_complete_permissions() {
    let f = Fixture::new().await;
    let song = f.song(0).await;
    let playlist = f.create(&song).await;
    let recipient = f
        .state
        .user_authorization
        .create_user("recipient", &"password".into())
        .await
        .unwrap();
    Fixture::map_user(&f.state, &recipient, &f.servers[0], "recipient-backend").await;
    let other = f
        .state
        .user_authorization
        .create_user("other", &"password".into())
        .await
        .unwrap();
    Fixture::map_user(&f.state, &other, &f.servers[1], "recipient-backend").await;
    Mock::given(method("GET"))
        .and(path("/Playlists/playlist"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "OpenAccess":false, "ItemIds":["song-0", "song-0"], "Shares":[
                {"UserId":"caller-0", "CanEdit":true},
                {"UserId":"recipient-backend", "CanEdit":false}
            ]
        })))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    let response = f
        .request(Method::GET, &format!("/Playlists/{playlist}"), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let details: Value = response.json().await.unwrap();
    assert_eq!(details["ItemIds"], json!([song, song]));
    assert_eq!(
        details["Shares"],
        json!([
            {"UserId":f.caller.id, "CanEdit":true}, {"UserId":recipient.id, "CanEdit":false}
        ])
    );
    Mock::given(method("GET"))
        .and(path("/Playlists/playlist/Users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"userId":"recipient-backend", "canEdit":false}
        ])))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    let response = f
        .request(Method::GET, &format!("/Playlists/{playlist}/Users"), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!([{ "userId":recipient.id, "canEdit":false }])
    );
    Mock::given(method("GET"))
        .and(path("/Playlists/playlist/Users/recipient-backend"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"UserId":"recipient-backend", "CanEdit":false})),
        )
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    let response = f
        .request(
            Method::GET,
            &format!("/Playlists/{playlist}/Users/{}", recipient.id),
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["UserId"],
        recipient.id
    );
}

#[tokio::test]
async fn playlist_sharing_reads_do_not_choose_between_ambiguous_account_aliases() {
    let f = Fixture::new().await;
    let song = f.song(0).await;
    let playlist = f.create(&song).await;
    let alias = f
        .state
        .user_authorization
        .create_user("alias", &"password".into())
        .await
        .unwrap();
    Fixture::map_user(&f.state, &alias, &f.servers[0], "caller-0").await;
    Mock::given(method("GET"))
        .and(path("/Playlists/playlist/Users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"UserId":"caller-0", "CanEdit":true}
        ])))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    let response = f
        .request(Method::GET, &format!("/Playlists/{playlist}/Users"), None)
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn incomplete_playlist_sharing_reads_fail_instead_of_returning_partial_lists() {
    let f = Fixture::new().await;
    let song = f.song(0).await;
    let playlist = f.create(&song).await;
    for (suffix, body) in [
        (
            "",
            json!({"ItemIds":["song-0"], "Shares":[
                {"UserId":"caller-0", "CanEdit":true}, {"UserId":"unmapped", "CanEdit":false}
            ]}),
        ),
        (
            "/Users",
            json!([
                {"UserId":"caller-0", "CanEdit":true}, {"UserId":"unmapped", "CanEdit":false}
            ]),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/Playlists/playlist{suffix}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&f.upstreams[0])
            .await;
        let response = f
            .request(Method::GET, &format!("/Playlists/{playlist}{suffix}"), None)
            .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(!response.text().await.unwrap().contains("unmapped"));
    }
}

#[tokio::test]
async fn encoded_container_paths_use_the_same_owner_for_validation_and_translation() {
    let f = Fixture::new().await;
    let movie = f.media(1, "movie", "Movie").await;
    let encoded = |id: &str| {
        id.bytes()
            .map(|byte| format!("%{byte:02X}"))
            .collect::<String>()
    };
    for (tag, backend) in [("Collections", "collection"), ("Playlists", "playlist")] {
        let mapping = f
            .state
            .media_storage
            .get_or_create_media_mapping(backend, &f.servers[1])
            .await
            .unwrap();
        Mock::given(method("POST"))
            .and(path(format!("/{tag}/{backend}/Items")))
            .and(query_param("Ids", "movie"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&f.upstreams[1])
            .await;
        let response = f
            .request(
                Method::POST,
                &format!(
                    "/{}/{}/%49tems?Ids={movie}",
                    encoded(tag),
                    encoded(&mapping.virtual_media_id)
                ),
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{tag}");
        if tag == "Playlists" {
            Mock::given(method("GET"))
                .and(path("/Playlists/playlist/Users"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                    {"UserId":"caller-1", "CanEdit":true}
                ])))
                .expect(1)
                .mount(&f.upstreams[1])
                .await;
            let response = f
                .request(
                    Method::GET,
                    &format!(
                        "/{}/{}/%55sers",
                        encoded(tag),
                        encoded(&mapping.virtual_media_id)
                    ),
                    None,
                )
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.json::<Value>().await.unwrap()[0]["UserId"],
                f.caller.id
            );
        }
    }
    assert!(f.upstreams[0]
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| request.method == "GET"));
}

#[tokio::test]
async fn invalid_playlist_sharing_responses_return_bad_gateway_not_mapping_conflicts() {
    let mut responses = [
        json!([{"CanEdit":true}]),
        json!([{"UserId":42}]),
        json!(null),
        json!({"Shares":"invalid"}),
    ]
    .into_iter()
    .map(|body| ResponseTemplate::new(200).set_body_json(body))
    .collect::<Vec<_>>();
    responses.extend([
        ResponseTemplate::new(200)
            .set_body_string("invalid json")
            .insert_header("Content-Type", "application/json"),
        ResponseTemplate::new(200).insert_header("Content-Type", "application/json"),
        ResponseTemplate::new(200).set_body_string("upstream HTML"),
    ]);
    for response in responses {
        let f = Fixture::new().await;
        let song = f.song(0).await;
        let playlist = f.create(&song).await;
        Mock::given(method("GET"))
            .and(path("/Playlists/playlist/Users"))
            .respond_with(response)
            .expect(1)
            .mount(&f.upstreams[0])
            .await;
        assert_eq!(
            f.request(Method::GET, &format!("/Playlists/{playlist}/Users"), None)
                .await
                .status(),
            StatusCode::BAD_GATEWAY
        );
    }
}

#[tokio::test]
async fn playlist_mapping_database_failures_are_internal_errors() {
    let f = Fixture::new().await;
    f.pool.close().await;
    let mut permissions = json!([{"UserId":"caller-0", "CanEdit":true}]);
    assert_eq!(
        f.state
            .process_response_json(
                &mut permissions,
                &f.servers[0],
                ResponseProcessingProfile::PlaylistPermissions,
                false,
                None
            )
            .await
            .unwrap_err(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn complete_playlist_sharing_can_be_saved_back_without_losing_recipients() {
    let f = Fixture::new().await;
    let song = f.song(0).await;
    let playlist = f.create(&song).await;
    let recipient = f
        .state
        .user_authorization
        .create_user("recipient", &"password".into())
        .await
        .unwrap();
    Fixture::map_user(&f.state, &recipient, &f.servers[0], "recipient-backend").await;
    let shares = json!([
        {"UserId":"caller-0", "CanEdit":true}, {"UserId":"recipient-backend", "CanEdit":false}
    ]);
    Mock::given(method("GET"))
        .and(path("/Playlists/playlist"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ItemIds":["song-0"], "Shares":shares})),
        )
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    let response = f
        .request(Method::GET, &format!("/Playlists/{playlist}"), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let read: Value = response.json().await.unwrap();
    Mock::given(method("POST"))
        .and(path("/Playlists/playlist"))
        .and(body_json(json!({"Ids":["song-0"], "Users":shares})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    assert_eq!(
        f.request(
            Method::POST,
            &format!("/Playlists/{playlist}"),
            Some(json!({
                "Ids":read["ItemIds"], "Users":read["Shares"]
            }))
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn collection_responses_continue_to_disable_deletion() {
    let f = Fixture::new().await;
    let anchor = f.media(0, "movie", "Movie").await;
    Mock::given(method("GET"))
        .and(path("/TestCollections"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Items":[{"Id":"collection", "Name":"Collection", "Type":"BoxSet", "CanDelete":true}]
        })))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    let response = f
        .request(
            Method::GET,
            &format!("/TestCollections?ParentId={anchor}"),
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["Items"][0]["CanDelete"],
        false
    );
}

#[tokio::test]
async fn movie_series_and_episode_playlists_route_to_their_owner() {
    let f = Fixture::new().await;
    let anchor = f.song(1).await;
    Mock::given(method("GET"))
        .and(path("/TestVideoItems"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Items":[
            {"Id":"movie", "Name":"Movie", "Type":"Movie"},
            {"Id":"series", "Name":"Series", "Type":"Series"},
            {"Id":"episode", "Name":"Episode", "Type":"Episode"}
        ]})))
        .mount(&f.upstreams[1])
        .await;
    let response = f
        .request(
            Method::GET,
            &format!("/TestVideoItems?ParentId={anchor}"),
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let media: Value = response.json().await.unwrap();
    let ids = media["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Id"].as_str().unwrap())
        .collect::<Vec<_>>();
    Mock::given(method("POST"))
        .and(path("/Playlists"))
        .and(body_json(json!({"Name":"Video", "Ids":["movie","series","episode"], "UserId":"caller-1", "MediaType":"Video"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Id":"video-playlist"})))
        .expect(1).mount(&f.upstreams[1]).await;
    let response = f
        .request(
            Method::POST,
            "/Playlists",
            Some(json!({
                "Name":"Video", "Ids":ids, "UserId":f.caller.id, "MediaType":"Video"
            })),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let created: Value = response.json().await.unwrap();
    let playlist = created["Id"].as_str().unwrap();

    Mock::given(method("POST"))
        .and(path("/Playlists/video-playlist"))
        .and(body_json(
            json!({"Name":"Renamed video", "Ids":["episode","movie"], "IsPublic":false}),
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    assert_eq!(
        f.request(
            Method::POST,
            &format!("/Playlists/{playlist}"),
            Some(json!({
                "Name":"Renamed video", "Ids":[ids[2],ids[0]], "IsPublic":false
            }))
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );

    Mock::given(method("GET"))
        .and(path("/Playlists/video-playlist/Items"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Items":[{"Id":"movie", "Type":"Movie", "PlaylistItemId":"video-entry"},
                {"Id":"episode", "Type":"Episode", "PlaylistItemId":"episode-entry"}],
            "TotalRecordCount":2, "StartIndex":0
        })))
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let response = f
        .request(Method::GET, &format!("/Playlists/{playlist}/Items"), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let items: Value = response.json().await.unwrap();
    assert_eq!(items["Items"][0]["Id"], ids[0]);
    assert_eq!(items["Items"][1]["Id"], ids[2]);
    let entry = items["Items"][0]["PlaylistItemId"].as_str().unwrap();
    Mock::given(method("POST"))
        .and(path("/Playlists/video-playlist/Items/video-entry/Move/1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    assert_eq!(
        f.request(
            Method::POST,
            &format!("/Playlists/{playlist}/Items/{entry}/Move/1"),
            None
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        f.upstreams[0]
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.method != "GET")
            .count(),
        0
    );
}

#[tokio::test]
async fn collection_creation_item_lookup_and_deletion_translate_ids() {
    let f = Fixture::new().await;
    let movie = f.media(1, "movie", "Movie").await;
    Mock::given(method("POST"))
        .and(path("/Collections"))
        .and(query_param("Ids", "movie"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Id":"collection"})))
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let response = f
        .request(
            Method::POST,
            &format!("/Collections?Name=Test&Ids={movie}"),
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let created: Value = response.json().await.unwrap();
    let collection = created["Id"].as_str().unwrap();
    assert_ne!(collection, "collection");
    Mock::given(method("GET"))
        .and(path("/Items/movie/Collections"))
        .and(query_param("UserId", "caller-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Items":[{"Id":"collection", "Name":"Test", "Type":"BoxSet"}],
            "TotalRecordCount":1, "StartIndex":0
        })))
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let response = f
        .request(
            Method::GET,
            &format!("/Items/{movie}/Collections?UserId={}", f.caller.id),
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let memberships: Value = response.json().await.unwrap();
    assert_eq!(memberships["Items"][0]["Id"], collection);
    Mock::given(method("DELETE"))
        .and(path("/Items/collection"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    assert_eq!(
        f.request(Method::DELETE, &format!("/Items/{collection}"), None)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        f.upstreams[0]
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.method != "GET")
            .count(),
        0
    );
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
    Mock::given(method("DELETE"))
        .and(path("/Playlists/playlist/Users/recipient-0"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    assert_eq!(
        f.request(
            Method::DELETE,
            &format!("/Playlists/{playlist}/Users/{}", recipient.id),
            None
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
