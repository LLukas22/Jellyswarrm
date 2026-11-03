//! #223: Kodi's unquoted, URL-encoded headers and Quick Connect session must
//! support synced item playback and the KodiSyncQueue plugin's identities.
use super::*;
use wiremock::{
    matchers::{body_json, header_regex, method, path, query_param},
    Mock, ResponseTemplate,
};

const BACKEND_USER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ITEM: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const REMOVED: &str = "cccccccccccccccccccccccccccccccc";
const CLIENT: &str = "MediaBrowser Client=Jellyfin%20for%20Kodi, Device=Kodi%20Linux, DeviceId=kodi-device, Version=2.2.0";

async fn quick_connect(f: &Fixture) -> (String, String) {
    // A fresh user has no pre-created device sessions and only owns the second
    // backend, so the generic best-server fallback cannot make this test pass.
    let user = f
        .state
        .user_authorization
        .create_user("kodi", &"password".into())
        .await
        .unwrap();
    let (_, server) = f
        .state
        .media_storage
        .get_media_mapping_with_server(&f.items[1])
        .await
        .unwrap()
        .unwrap();
    f.state
        .user_authorization
        .add_server_mapping(&user.id, &server, "kodi", &"password".into(), None)
        .await
        .unwrap();
    f.state
        .server_storage
        .update_server_media_streaming_mode(server.id, MediaStreamingMode::Redirect)
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/Users/AuthenticateByName"))
        .and(body_json(json!({"Username":"kodi","Pw":"password"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "User":{"Id":BACKEND_USER,"Name":"kodi","ServerId":"backend",
                "Policy":{"IsAdministrator":false,"SyncPlayAccess":"None"}},
            "SessionInfo":{"UserId":BACKEND_USER,"UserName":"kodi","ServerId":"backend"},
            "AccessToken":"kodi-backend-token","ServerId":"backend"
        })))
        .expect(1)
        .mount(&f.upstreams[1])
        .await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/QuickConnect/Initiate", f.url))
        .header("Authorization", CLIENT)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let initiation: Value = response.json().await.unwrap();
    let response = client
        .post(format!("{}/QuickConnect/Authorize", f.url))
        .header("X-MediaBrowser-Token", &user.virtual_key)
        .query(&[("code", initiation["Code"].as_str().unwrap())])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = client
        .post(format!("{}/Users/AuthenticateWithQuickConnect", f.url))
        .header("Authorization", CLIENT)
        .json(&json!({"Secret":initiation["Secret"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let authentication: Value = response.json().await.unwrap();
    assert_eq!(authentication["User"]["Id"], user.id.as_str());
    assert_eq!(authentication["AccessToken"], user.virtual_key);
    (
        format!("{CLIENT}, UserId={}, Token={}", user.id, user.virtual_key),
        authentication["User"]["Id"].as_str().unwrap().to_owned(),
    )
}

fn playback_authorization(user: &str, token: &str, device_id: &str) -> String {
    format!(
        "MediaBrowser Client=Jellyfin%20for%20Kodi, Device=Unknown%20Device, DeviceId={device_id}, Version=0.0.0, UserId={user}, Token={token}"
    )
}

#[tokio::test]
async fn kodi_quick_connect_synced_item_plays_in_redirect_mode() {
    let f = Fixture::new().await;
    f.healthy().await;
    let (authorization, user) = quick_connect(&f).await;
    let upstream_item = json!({
        "Id":ITEM,"Name":"Kodi movie","Type":"Movie",
        "MediaSources":[{"Id":ITEM,"Type":"Default","SupportsDirectPlay":true}]
    });
    for (endpoint, body) in [
        (
            format!("/Users/{BACKEND_USER}/Items"),
            json!({"Items":[upstream_item.clone()],"TotalRecordCount":1,"StartIndex":0}),
        ),
        (format!("/Users/{BACKEND_USER}/Items/{ITEM}"), upstream_item),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .and(header_regex("authorization", "kodi-backend-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&f.upstreams[1])
            .await;
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = client
        .get(format!(
            "{}/Users/{user}/Items?Recursive=true&IncludeItemTypes=Movie",
            f.url
        ))
        .header("Authorization", &authorization)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let synced: Value = response.json().await.unwrap();
    let item = synced["Items"][0]["Id"].as_str().unwrap();
    assert_ne!(item, ITEM);
    // Playback runs in a separate Kodi plugin process without config.app().
    // It keeps the login token, but sends placeholders instead of the synced
    // device metadata. Using CLIENT here missed the original #223 failure.
    let token = Authorization::parse(&authorization).unwrap().token.unwrap();
    let authorization = playback_authorization(&user, &token, "Unknown%20Device%20id");
    let response = client
        .get(format!("{}/Users/{user}/Items/{item}", f.url))
        .header("Authorization", &authorization)
        .header("User-Agent", "Jellyfin for Kodi/0.0.0")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail: Value = response.json().await.unwrap();
    assert_eq!(detail["Id"], item);
    let source = detail["MediaSources"][0]["Id"].as_str().unwrap();
    Mock::given(method("POST"))
        .and(path(format!("/Items/{ITEM}/PlaybackInfo")))
        .and(header_regex("authorization", "kodi-backend-token"))
        .and(body_json(
            json!({"UserId":BACKEND_USER,"DeviceProfile":{},"AutoOpenLiveStream":true}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "PlaySessionId":"kodi-play","MediaSources":[{"Id":ITEM,"Type":"Default"}]
        })))
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let response = client
        .post(format!("{}/Items/{item}/PlaybackInfo", f.url))
        .header("Authorization", &authorization)
        .header("User-Agent", "Jellyfin for Kodi/0.0.0")
        .json(&json!({"UserId":user,"DeviceProfile":{},"AutoOpenLiveStream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let playback: Value = response.json().await.unwrap();
    assert_eq!(playback["MediaSources"][0]["Id"], source);
    Mock::given(method("GET"))
        .and(path(format!("/Videos/{ITEM}/stream")))
        .and(query_param("MediaSourceId", ITEM))
        .and(query_param("ApiKey", "kodi-backend-token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Kodi media"))
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let response = client
        .get(format!(
            "{}/Videos/{item}/stream?Static=true&MediaSourceId={source}",
            f.url
        ))
        .header("Authorization", &authorization)
        .header("User-Agent", "Jellyfin for Kodi/0.0.0")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    let location = response.headers()["Location"].to_str().unwrap();
    let response = reqwest::get(location).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "Kodi media");
    f.upstreams[1].verify().await;
    assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn kodi_placeholder_device_cannot_borrow_another_users_sessions_or_ignore_a_real_device() {
    let f = Fixture::new().await;
    f.healthy().await;
    let (authorization, user) = quick_connect(&f).await;
    let token = Authorization::parse(&authorization).unwrap().token.unwrap();
    let client = reqwest::Client::new();
    let before = f.upstreams[1].received_requests().await.unwrap().len();
    // Keep the abbreviated-header lookup strict even for a placeholder ID.
    let response = client
        .get(format!("{}/Users/{user}/Items/{}", f.url, f.items[1]))
        .header(
            "Authorization",
            format!("MediaBrowser DeviceId=Unknown%20Device%20id, Token={token}"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        f.upstreams[1].received_requests().await.unwrap().len(),
        before
    );
    // Unrecognized/absent credentials retain backend passthrough behavior, but
    // must never be upgraded to the real user's stored backend credentials.
    Mock::given(method("GET"))
        .and(|request: &wiremock::Request| {
            request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .is_none_or(|value| !value.contains("kodi-backend-token"))
        })
        .respond_with(ResponseTemplate::new(401))
        .expect(2)
        .mount(&f.upstreams[1])
        .await;
    for (device_id, token, expected) in [
        (
            "different-real-device",
            token.as_str(),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            "Unknown%20Device%20id",
            f.other.virtual_key.as_str(),
            StatusCode::FORBIDDEN,
        ),
        (
            "Unknown%20Device%20id",
            "invalid-token",
            StatusCode::UNAUTHORIZED,
        ),
        ("Unknown%20Device%20id", "", StatusCode::UNAUTHORIZED),
    ] {
        let response = client
            .get(format!("{}/Users/{user}/Items/{}", f.url, f.items[1]))
            .header(
                "Authorization",
                playback_authorization(&user, token, device_id),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "device: {device_id}");
    }
    assert_eq!(
        f.upstreams[1].received_requests().await.unwrap().len(),
        before + 2
    );
    f.upstreams[1].verify().await;
    assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn placeholder_and_user_agent_devices_use_only_unexpired_token_owner_sessions() {
    let f = Fixture::new().await;
    f.healthy().await;
    let original = f.original(0).await;
    Mock::given(method("GET"))
        .and(path(format!("/Users/backend-user-0/Items/{original}")))
        .and(header_regex("authorization", "backend-token-0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Id":original,"Name":"Owned movie","Type":"Movie"
        })))
        .expect(4)
        .mount(&f.upstreams[0])
        .await;
    let client = reqwest::Client::new();
    for (header, value) in [
        (
            "Authorization",
            playback_authorization(&f.user.id, &f.user.virtual_key, "unknown-device-id"),
        ),
        (
            "Authorization",
            playback_authorization(&f.user.id, &f.user.virtual_key, "n/a"),
        ),
        (
            "X-Emby-Authorization",
            playback_authorization(&f.user.id, &f.user.virtual_key, "Unknown%20Device%20id"),
        ),
        ("X-Emby-Token", f.user.virtual_key.clone()),
    ] {
        let response = client
            .get(format!(
                "{}/Users/{}/Items/{}",
                f.url, f.user.id, f.items[0]
            ))
            .header(header, value)
            .header("User-Agent", "Jellyfin for Kodi/0.0.0")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{header}");
        assert_eq!(response.json::<Value>().await.unwrap()["Id"], f.items[0]);
    }
    sqlx::query("UPDATE authorization_sessions SET expires_at = ? WHERE user_id = ?")
        .bind(chrono::Utc::now() - chrono::Duration::seconds(1))
        .bind(f.user.id.as_str())
        .execute(&f.pool)
        .await
        .unwrap();
    let response = client
        .get(format!(
            "{}/Users/{}/Items/{}",
            f.url, f.user.id, f.items[0]
        ))
        .header(
            "Authorization",
            playback_authorization(&f.user.id, &f.user.virtual_key, "Unknown%20Device%20id"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    f.upstreams[0].verify().await;
    assert!(f.upstreams[1].received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn kodi_sync_queue_translates_user_and_changed_item_ids() {
    let f = Fixture::new().await;
    f.healthy().await;
    let (authorization, user) = quick_connect(&f).await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/Jellyfin.Plugin.KodiSyncQueue/{BACKEND_USER}/GetItems"
        )))
        .and(header_regex("authorization", "kodi-backend-token"))
        .and(query_param("LastUpdateDT", "2026-11-02T00:00:00Z"))
        .and(query_param("filter", "movies,tvshows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ItemsAdded":[ITEM],"ItemsUpdated":[ITEM],"ItemsRemoved":[REMOVED],
            "UserDataChanged":[{"ItemId":ITEM,"Played":true}]
        })))
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let response = reqwest::Client::new()
        .get(format!(
            "{}/Jellyfin.Plugin.KodiSyncQueue/{user}/GetItems",
            f.url
        ))
        .header("Authorization", &authorization)
        .query(&[
            ("LastUpdateDT", "2026-11-02T00:00:00Z"),
            ("filter", "movies,tvshows"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: Value = response.json().await.unwrap();
    let added = result["ItemsAdded"][0].as_str().unwrap();
    assert_ne!(added, ITEM);
    assert_eq!(result["ItemsUpdated"][0], added);
    assert_eq!(result["UserDataChanged"][0]["ItemId"], added);
    assert_eq!(result["UserDataChanged"][0]["Played"], true);
    assert_ne!(result["ItemsRemoved"][0], REMOVED);
    for (field, original) in [
        ("ItemsAdded", ITEM),
        ("ItemsUpdated", ITEM),
        ("ItemsRemoved", REMOVED),
    ] {
        let (mapping, server) = f
            .state
            .media_storage
            .get_media_mapping_with_server(result[field][0].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mapping.original_media_id, original);
        assert_eq!(
            server.url.as_str().trim_end_matches('/'),
            f.upstreams[1].uri()
        );
    }
    // Queue IDs must be usable by the same legacy item-detail call as a full
    // library sync, including items first discovered through the plugin.
    Mock::given(method("GET"))
        .and(path(format!("/Users/{BACKEND_USER}/Items/{ITEM}")))
        .and(header_regex("authorization", "kodi-backend-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"Id":ITEM,"Name":"Queued movie","Type":"Movie"})),
        )
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let response = reqwest::Client::new()
        .get(format!("{}/Users/{user}/Items/{added}", f.url))
        .header("Authorization", &authorization)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail: Value = response.json().await.unwrap();
    assert_eq!(detail["Id"], added);
    f.upstreams[1].verify().await;
    assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
}
