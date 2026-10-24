//! HTTP/WebSocket integration tests with real SQLite authorization and zero-upstream assertions.
use super::*;
use crate::{
    auth::service::Device,
    http_tests::support::{device, Fixture},
    models::Authorization,
};
use axum::http::StatusCode;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::{
    sync::mpsc,
    time::{timeout, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

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
    assert_eq!(msg["Data"]["ControllingUserId"], f.user.id.as_str());
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
        assert_eq!(msg["Data"]["ControllingUserId"], f.user.id.as_str());
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
        .all(|s| s["UserId"] == f.user.id.as_str()));
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
async fn controllable_user_filter_accepts_uuid_formats_without_granting_cross_user_access() {
    let f = Fixture::new().await;
    let _tv = f.socket("tv").await;
    f.capabilities().await;
    let expected = f.list().await;
    assert!(!expected.is_empty());
    let uuid = uuid::Uuid::parse_str(&f.user.id).unwrap();
    for id in [
        uuid.simple().to_string(),
        uuid.hyphenated().to_string(),
        uuid.simple().to_string().to_uppercase(),
        uuid.hyphenated().to_string().to_uppercase(),
    ] {
        let path = format!("/Sessions?ControllableByUserId={id}");
        let response = f
            .request(reqwest::Method::GET, &path, &f.user, "controller")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sessions = response.json::<Vec<Value>>().await.unwrap();
        assert_eq!(sessions.len(), expected.len());
        assert!(sessions
            .iter()
            .all(|session| session["UserId"] == f.user.id.as_str()));
        assert_eq!(
            f.request(reqwest::Method::GET, &path, &f.other, "other")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
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
    assert_eq!(msg["Data"]["ControllingUserId"], f.user.id.as_str());
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
    let auth = crate::http::preprocessing::JellyfinAuthorization::Authorization(Authorization {
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
        crate::http::preprocessing::request_device(&request, Some(&auth))
            .unwrap()
            .device_id,
        "web"
    );
    let request = reqwest::Request::new(
        reqwest::Method::GET,
        url::Url::parse("http://local/socket?deviceId=tv").unwrap(),
    );
    let token = crate::http::preprocessing::JellyfinAuthorization::XEmbyToken("token".into());
    assert_eq!(
        crate::http::preprocessing::request_device(&request, Some(&token))
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
