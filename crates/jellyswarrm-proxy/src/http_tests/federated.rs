//! Route/admin assertions retained at the HTTP boundary.
use crate::application::catalog::tests::{library_items, setup};
use crate::auth::service::UserAuthorizationService;
use crate::http::handlers::federated::*;
use axum::extract::State;
use hyper::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;
use wiremock::{
    matchers::{method, path},
    Mock, ResponseTemplate,
};

#[tokio::test]
async fn routing_storage_failure_returns_internal_error_without_contacting_a_backend() {
    let fixture = super::support::Fixture::new().await;
    fixture.healthy().await;
    sqlx::query(
        "ALTER TABLE media_mappings RENAME COLUMN virtual_media_id TO unavailable_virtual_id",
    )
    .execute(&fixture.pool)
    .await
    .unwrap();
    for path in [
        format!("/Items/{}", fixture.items[0]),
        format!("/Items/{}/Images/Primary", fixture.items[0]),
    ] {
        let response = fixture
            .request(reqwest::Method::GET, &path, &fixture.user, "tv")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{path}"
        );
    }
    for upstream in &fixture.upstreams {
        assert!(upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| { !request.url.path().starts_with("/Items/") }));
    }
}

#[tokio::test]
async fn post_body_lookup_failure_is_internal_error_but_invalid_selection_is_bad_request() {
    let fixture = super::support::Fixture::new().await;
    fixture.healthy().await;
    let invalid = fixture
        .request(reqwest::Method::POST, "/BodyLookup", &fixture.user, "tv")
        .json(&json!({"Ids": ["unknown"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

    sqlx::query(
        "ALTER TABLE media_mappings RENAME COLUMN virtual_media_id TO unavailable_virtual_id",
    )
    .execute(&fixture.pool)
    .await
    .unwrap();
    for body in [
        json!({"ItemId": fixture.items[0]}),
        json!({"Ids": [fixture.items[0], fixture.items[1]]}),
    ] {
        let response = fixture
            .request(reqwest::Method::POST, "/BodyLookup", &fixture.user, "tv")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let message = response.text().await.unwrap();
        assert!(!message.contains("unavailable_virtual_id"));
        assert!(!message.contains("media_mappings"));
    }
    for upstream in &fixture.upstreams {
        assert!(upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method != "POST"));
    }
}
#[tokio::test]
async fn http_catalog_input_validates_offline_ids_without_initial_backend_selection() {
    use tower::ServiceExt;

    let (state, _, sessions, upstreams) = setup().await;
    let user = state
        .user_authorization
        .create_user("catalog-input", &"password".into())
        .await
        .unwrap();
    let authorization = crate::models::Authorization {
        client: "test".into(),
        device: "test".into(),
        device_id: "test".into(),
        version: "1".into(),
        token: None,
    };
    for (_, server) in &sessions {
        state
            .user_authorization
            .add_server_mapping(&user.id, server, "catalog-input", &"password".into(), None)
            .await
            .unwrap();
        state
            .user_authorization
            .store_authorization_session(
                &user.id,
                server,
                &authorization,
                "upstream-token".into(),
                "backend-user".into(),
                None,
            )
            .await
            .unwrap();
    }
    Mock::given(path("/System/Info/Public"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ServerName":"Online"})))
        .mount(&upstreams[1])
        .await;
    Mock::given(path("/Items"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Items":[{"Id":"movie","Type":"Movie"}], "TotalRecordCount":1,"StartIndex":0,
        })))
        .mount(&upstreams[1])
        .await;
    state.server_storage.check_servers_health().await;
    let mut facets = Vec::new();
    for (_, server) in &sessions {
        facets.push(
            state
                .media_storage
                .get_or_create_media_mapping("genre", server)
                .await
                .unwrap()
                .virtual_media_id,
        );
    }
    let app = axum::Router::new()
        .route(
            "/Items",
            axum::routing::get(get_items_from_all_servers_if_not_restricted),
        )
        .with_state(state.clone());
    let send = |uri: String| {
        let app = app.clone();
        let auth = format!("MediaBrowser Client=\"test\", Device=\"test\", DeviceId=\"test\", Version=\"1\", Token=\"{}\"", user.virtual_key);
        async move {
            app.oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .header("Authorization", auth)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    let response = send(format!("/Items?GenreIds={},{}", facets[0], facets[1])).await;
    assert_eq!(response.status(), StatusCode::OK);
    let online = upstreams[1].received_requests().await.unwrap();
    let catalog_requests = online
        .iter()
        .filter(|request| request.url.path() == "/Items")
        .collect::<Vec<_>>();
    assert_eq!(catalog_requests.len(), 1);
    assert!(catalog_requests[0]
        .url
        .query_pairs()
        .any(|(key, value)| key == "GenreIds" && value == "genre"));
    let before = online.len();

    // A concrete offline owner still has the legacy FORBIDDEN contract,
    // rather than being redirected to the healthy source.
    assert_eq!(
        send(format!("/Items?SeriesId={}", facets[0]))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        upstreams[1].received_requests().await.unwrap().len(),
        before
    );

    // Removing access to the offline source rejects its IDs before any fan-out.
    let mapping = state
        .user_authorization
        .get_server_mapping_by_server_id(&user.id, sessions[0].1.id)
        .await
        .unwrap()
        .unwrap();
    state
        .user_authorization
        .delete_server_mapping(mapping.id)
        .await
        .unwrap();
    assert_eq!(
        send(format!("/Items?GenreIds={},{}", facets[0], facets[1]))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        upstreams[1].received_requests().await.unwrap().len(),
        before
    );

    // All unavailable is still 503, including an otherwise federated query.
    upstreams[1].reset().await;
    state.server_storage.check_servers_health().await;
    let before = upstreams[1].received_requests().await.unwrap().len();
    for uri in [
        "/Items".to_string(),
        format!("/Items?SeriesId={}", facets[1]),
    ] {
        assert_eq!(send(uri).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
    assert_eq!(
        upstreams[1].received_requests().await.unwrap().len(),
        before
    );
    assert!(upstreams[0]
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| request.url.path() == "/System/Info/Public"));
}

#[tokio::test]
async fn admin_library_page_discovers_mapped_users_without_prior_catalog_requests() {
    use axum::response::IntoResponse;
    let (mut state, pool, sessions, upstreams) = setup().await;
    state.user_authorization = Arc::new(UserAuthorizationService::with_mapping_key(
        pool,
        crate::auth::encryption::MappingEncryptionKey::from_session_key(&[7; 64]).unwrap(),
        (&state.get_admin_password().await).into(),
    ));
    let server = &sessions[0].1;
    Mock::given(method("GET"))
        .and(path("/System/Info/Public"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ServerName":"Test"})))
        .mount(&upstreams[0])
        .await;
    for (index, username) in ["restricted", "complete"].into_iter().enumerate() {
        let user = state
            .user_authorization
            .create_user(username, &"password".into())
            .await
            .unwrap();
        state
            .user_authorization
            .add_server_mapping(&user.id, server, username, &"password".into(), None)
            .await
            .unwrap();
        let backend_id = format!("user-{index}");
        Mock::given(method("POST")).and(path("/Users/AuthenticateByName"))
            .and(wiremock::matchers::body_json(json!({"Username": username, "Pw": "password"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "User": {"Id": backend_id, "Name": username}, "AccessToken": "token", "ServerId":"test"
            }))).mount(&upstreams[0]).await;
        let mut libraries = library_items(if index == 0 { 1 } else { 25 });
        // Regression for #202: continuation entries are visible but locked if
        // their inventory DTO omits CollectionType. Details can supply it.
        for library in libraries.iter_mut().skip(20) {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/Users/{backend_id}/Items/{}",
                    library["Id"].as_str().unwrap()
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(library.clone()))
                .expect(1)
                .mount(&upstreams[0])
                .await;
            library.as_object_mut().unwrap().remove("CollectionType");
        }
        Mock::given(method("GET"))
            .and(path(format!("/Users/{backend_id}/Views")))
            .respond_with(move |request: &wiremock::Request| {
                let start = request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == "StartIndex")
                    .and_then(|(_, value)| value.parse::<usize>().ok())
                    .unwrap_or(0);
                ResponseTemplate::new(200).set_body_json(json!({
                    "Items": libraries.iter().skip(start).take(20).collect::<Vec<_>>(),
                    "TotalRecordCount": libraries.len()
                }))
            })
            .mount(&upstreams[0])
            .await;
    }
    assert!(state
        .virtual_library_service
        .list_discovered_libraries()
        .await
        .unwrap()
        .is_empty());
    let response = crate::ui::admin::libraries::library_groups_list(State(state.clone()))
        .await
        .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(html.matches("data-library-card").count(), 25, "{html}");
    assert!(html.contains("Library 24"));
    assert!(!html.contains("role=\"alert\""), "{html}");
    assert!(html.contains("id=\"library-discovery-errors\""));
    assert_eq!(
        state
            .virtual_library_service
            .list_discovered_libraries()
            .await
            .unwrap()
            .len(),
        25
    );

    // Editing groups uses the completed cache, not another upstream discovery.
    use crate::ui::admin::libraries::{
        assign_library, create_group, delete_group, remove_member, rename_group, AssignLibraryForm,
        CreateGroupForm, RemoveMemberForm, RenameGroupForm,
    };
    let request_count = upstreams[0].received_requests().await.unwrap().len();
    let created = create_group(
        State(state.clone()),
        axum::Form(CreateGroupForm {
            name: "Test group".into(),
        }),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let body = axum::body::to_bytes(created.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(html.matches("data-can-drag=\"true\"").count(), 25);
    assert_eq!(
        html.matches("class=\"library-assignment-form\"").count(),
        25
    );
    let mut responses = Vec::new();
    let group_id = state.virtual_library_service.list_groups().await.unwrap()[0]
        .virtual_id
        .clone();
    responses.push(
        assign_library(
            State(state.clone()),
            axum::Form(AssignLibraryForm {
                group_virtual_id: group_id.clone(),
                server_id: server.id.as_i64(),
                library_id: "library-24".into(),
            }),
        )
        .await,
    );
    responses.push(
        rename_group(
            State(state.clone()),
            axum::extract::Path(group_id.clone()),
            axum::Form(RenameGroupForm {
                name: "Renamed group".into(),
            }),
        )
        .await,
    );
    responses.push(
        remove_member(
            State(state.clone()),
            axum::extract::Path(group_id.clone()),
            axum::Form(RemoveMemberForm {
                server_id: server.id.as_i64(),
                library_id: "library-24".into(),
            }),
        )
        .await,
    );
    responses.push(delete_group(State(state.clone()), axum::extract::Path(group_id)).await);
    for response in responses {
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(html.matches("data-library-card").count(), 25);
        assert!(html.contains("Library 24"));
        assert!(!html.contains("id=\"library-discovery-errors\""));
    }
    assert_eq!(
        upstreams[0].received_requests().await.unwrap().len(),
        request_count
    );

    // An explicit refresh still contacts upstreams and reports failures.
    upstreams[0].reset().await;
    let response = crate::ui::admin::libraries::library_groups_list(State(state.clone()))
        .await
        .into_response();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(html.matches("data-library-card").count(), 25);
    assert!(html.contains("role=\"alert\""));
    assert_eq!(
        state
            .virtual_library_service
            .list_discovered_libraries()
            .await
            .unwrap()
            .len(),
        25
    );
}

// Exercise the production HTTP handler and preprocessing, not just cmp_by.
// Jellyfin only includes SortName and DateCreated when requested in Fields.
async fn check_web_album_sorting(sort_by: &str, ascending_names: &[&str]) {
    let (state, _pool, sessions, upstreams) = setup().await;
    let user = state
        .user_authorization
        .create_user("album-viewer", &"password".into())
        .await
        .unwrap();
    let authorization = crate::models::Authorization {
        client: "album-test".into(),
        device: "test".into(),
        device_id: "album-test".into(),
        version: "1".into(),
        token: None,
    };
    let albums = [
        json!([
            {"Id":"a", "Type":"MusicAlbum", "Name":"The Zebra", "SortName":"aardvark", "AlbumArtist":"Shared artist", "DateCreated":"2026-01-01T00:00:00Z"},
            {"Id":"c", "Type":"MusicAlbum", "Name":"Omega", "SortName":"omega", "AlbumArtist":"Z artist", "DateCreated":"2026-03-01T00:00:00Z"}
        ]),
        json!([
            {"Id":"b", "Type":"MusicAlbum", "Name":"Alpha", "SortName":"zulu", "AlbumArtist":"Shared artist", "DateCreated":"2026-01-01T00:00:00Z"},
            {"Id":"d", "Type":"MusicAlbum", "Name":"Beta", "SortName":"beta", "AlbumArtist":"A artist", "DateCreated":"2026-02-01T00:00:00Z"}
        ]),
    ];
    for (index, (_, server)) in sessions.iter().enumerate() {
        state
            .user_authorization
            .add_server_mapping(&user.id, server, "album-viewer", &"password".into(), None)
            .await
            .unwrap();
        state
            .user_authorization
            .store_authorization_session(
                &user.id,
                server,
                &authorization,
                "upstream-token".into(),
                "backend-user".into(),
                None,
            )
            .await
            .unwrap();
        Mock::given(method("GET"))
            .and(path("/System/Info/Public"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ServerName":"Test"})))
            .mount(&upstreams[index])
            .await;
        let albums = albums[index].as_array().unwrap().clone();
        Mock::given(method("GET"))
            .and(path("/Items"))
            .respond_with(move |request: &wiremock::Request| {
                let fields = request
                    .url
                    .query_pairs()
                    .filter(|(key, _)| key.eq_ignore_ascii_case("Fields"))
                    .flat_map(|(_, value)| value.split(',').map(str::to_owned).collect::<Vec<_>>())
                    .collect::<Vec<_>>();
                let mut items = albums.clone();
                for item in &mut items {
                    for field in ["SortName", "DateCreated"] {
                        if !fields
                            .iter()
                            .any(|requested| requested.eq_ignore_ascii_case(field))
                        {
                            item.as_object_mut().unwrap().remove(field);
                        }
                    }
                }
                ResponseTemplate::new(200).set_body_json(json!({
                    "Items":items, "TotalRecordCount":items.len(), "StartIndex":0
                }))
            })
            .mount(&upstreams[index])
            .await;
    }
    state.server_storage.check_servers_health().await;
    let app = axum::Router::new()
        .route(
            "/Items",
            axum::routing::get(
                crate::http::handlers::federated::get_items_from_all_servers_if_not_restricted,
            ),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // Modern web requests use camelCase and don't request SortName. Also cover
    // legacy PascalCase requests and pagination after the global sort.
    for (sort_key, order_key, fields_key) in [
        ("sortBy", "sortOrder", "fields"),
        ("SortBy", "SortOrder", "Fields"),
    ] {
        for order in ["Ascending", "Descending"] {
            let mut expected = ascending_names.to_vec();
            if order == "Descending" {
                expected.reverse();
            }
            for (start, limit) in [(0, 4), (1, 2)] {
                let mut request = state.reqwest_client.get(format!("{base_url}/Items"))
                    .header("Authorization", format!("MediaBrowser Client=\"album-test\", Device=\"test\", DeviceId=\"album-test\", Version=\"1\", Token=\"{}\"", user.virtual_key))
                    .query(&[
                        ("userId", user.id.as_str()), ("includeItemTypes", "MusicAlbum"),
                        ("recursive", "true"), (order_key, order),
                        (fields_key, if sort_key == "sortBy" { "PrimaryImageAspectRatio" } else { "PrimaryImageAspectRatio,SortName" }),
                        ("startIndex", &start.to_string()), ("limit", &limit.to_string()),
                    ]);
                if sort_key == "sortBy" {
                    // The Jellyfin SDK encodes array-valued sorts as repeated keys.
                    for field in sort_by.split(',') {
                        request = request.query(&[(sort_key, field)]);
                    }
                } else {
                    request = request.query(&[(sort_key, sort_by)]);
                }
                let response = request.send().await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let response: Value = response.json().await.unwrap();
                let names = response["Items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| {
                        item["Name"]
                            .as_str()
                            .unwrap()
                            .split(" [Server ")
                            .next()
                            .unwrap()
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    names,
                    expected[start..start + limit],
                    "{sort_by} {order}, start={start}: {response}"
                );
                assert_eq!(response["TotalRecordCount"], 4);
                assert_eq!(response["StartIndex"], start);
            }
        }
    }
    task.abort();
}

#[tokio::test]
async fn web_album_name_sorting_http_integration() {
    check_web_album_sorting("SortName", &["The Zebra", "Beta", "Omega", "Alpha"]).await;
}

#[tokio::test]
async fn web_album_date_added_sorting_http_integration() {
    check_web_album_sorting(
        "DateCreated,SortName",
        &["The Zebra", "Alpha", "Beta", "Omega"],
    )
    .await;
}

#[tokio::test]
async fn web_album_artist_sorting_http_integration() {
    check_web_album_sorting(
        "AlbumArtist,SortName",
        &["Beta", "The Zebra", "Alpha", "Omega"],
    )
    .await;
}
