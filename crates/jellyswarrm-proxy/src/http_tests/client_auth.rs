//! Client login, saved-token use and revocation. Source revisions are in README.md.
use super::*;
use reqwest::{Client, RequestBuilder};
use wiremock::{
    matchers::{body_json, header_regex, method, path},
    Mock, ResponseTemplate,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum App {
    BundledWeb(&'static str),
    JellyfinWeb,
    AndroidTv,
    Findroid,
    Streamyfin,
    Swiftfin,
    Finamp,
    Roku,
    Kodi,
    MpvShim,
}

use App::*;

// Use literal client wire formats, independent of the proxy's authorization renderer.
impl App {
    fn authorization(self, device_id: &str, user: &str, token: Option<&str>) -> String {
        let quoted_token = token
            .map(|t| format!(", Token=\"{t}\""))
            .unwrap_or_default();
        let unquoted_token = token.map(|t| format!(", Token={t}")).unwrap_or_default();
        match self {
            BundledWeb(name) => format!("MediaBrowser Client=\"{name}\", Device=\"Test%20Device\", DeviceId=\"{device_id}\", Version=\"1\"{quoted_token}"),
            JellyfinWeb | Streamyfin => {
                let app = if self == JellyfinWeb { "Jellyfin%20Web" } else { "Streamyfin" };
                format!("MediaBrowser Client=\"{app}\", Device=\"Test%20Device\", DeviceId=\"{device_id}\", Version=\"1\", Token=\"{}\"", token.unwrap_or_default())
            }
            AndroidTv | Findroid => {
                let app = if self == AndroidTv { "Jellyfin+for+Android+TV" } else { "Findroid" };
                format!("MediaBrowser Client=\"{app}\", Version=\"1\", DeviceId=\"{device_id}\", Device=\"Test+Device\"{quoted_token}")
            }
            Swiftfin => format!("MediaBrowser DeviceId={device_id}, Device=Apple%20TV, Client=Swiftfin, Version=1{unquoted_token}"),
            Finamp => {
                let identity = token.map(|t| format!("UserId=\"{user}\", Token=\"{t}\", ")).unwrap_or_default();
                format!("MediaBrowser {identity}Client=\"Finamp\", Device=\"Test Phone\",DeviceId=\"{device_id}\", Version=\"1\"")
            }
            Roku => {
                let identity = token.map(|_| format!(", UserId=\"{user}\"")).unwrap_or_default();
                format!("MediaBrowser Client=\"Jellyfin Roku\", Device=\"Living Room (Ultra)\", Version=\"1\"{identity}, DeviceId=\"{device_id}\"{quoted_token}")
            }
            Kodi => format!("MediaBrowser Client=Jellyfin%20for%20Kodi, Device=Kodi%20Linux, DeviceId={device_id}, Version=2.2.0{unquoted_token}"),
            MpvShim => format!("MediaBrowser Client=\"Jellyfin MPV Shim\", Device=\"Test PC\", DeviceId=\"{device_id}\", Version=\"3.3.0\"{quoted_token}"),
        }
    }

    fn views_path(self, user: &str) -> String {
        match self {
            Finamp | Kodi | MpvShim => format!("/Users/{user}/Views"),
            _ => format!("/UserViews?userId={user}"),
        }
    }
}

async fn json_ok(request: RequestBuilder) -> Value {
    let response = request.send().await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        response.url().path()
    );
    response.json().await.unwrap()
}

async fn quick_connect(f: &Fixture, client: &Client, app: App, authorization: &str) -> Value {
    let url = &f.url;
    assert_eq!(
        json_ok(client.get(format!("{url}/QuickConnect/Enabled"))).await,
        true
    );
    let mut initiate = client
        .post(format!("{url}/QuickConnect/Initiate"))
        .header("Authorization", authorization);
    if app == Kodi {
        initiate = initiate.json(&json!({}));
    }
    let challenge = json_ok(initiate).await;
    let code = challenge["Code"].as_str().unwrap();
    let secret = challenge["Secret"].as_str().unwrap();
    assert!(!code.is_empty() && !secret.is_empty());
    assert_eq!(challenge["Authenticated"], false);
    assert_eq!(challenge["DeviceId"], "install-id");
    assert_ne!(challenge["AppName"].as_str().unwrap(), "Unknown App");
    chrono::DateTime::parse_from_rfc3339(challenge["DateAdded"].as_str().unwrap()).unwrap();
    let query = if matches!(app, BundledWeb(_) | Streamyfin | Finamp | Kodi) {
        "Secret"
    } else {
        "secret"
    };
    let poll = || {
        let request = client
            .get(format!("{url}/QuickConnect/Connect"))
            .query(&[(query, secret)]);
        if app == Streamyfin {
            request
        } else {
            request.header("Authorization", authorization)
        }
    };
    assert_eq!(json_ok(poll()).await["Authenticated"], false);
    assert_eq!(
        json_ok(
            client
                .post(format!("{url}/QuickConnect/Authorize"))
                .header("X-Emby-Token", &f.user.fixture_token)
                .query(&[("code", code)])
        )
        .await,
        true
    );
    assert_eq!(json_ok(poll()).await["Authenticated"], true);
    let body_key = if matches!(app, Roku | Streamyfin) {
        "secret"
    } else {
        "Secret"
    };
    json_ok(
        client
            .post(format!("{url}/Users/AuthenticateWithQuickConnect"))
            .header("Authorization", authorization)
            .json(&json!({(body_key):secret})),
    )
    .await
}

async fn mount_backends(f: &Fixture, app: App) {
    for (index, backend) in f.upstreams.iter().enumerate() {
        let user = format!("backend-user-{index}");
        let token = format!("issued-token-{index}");
        let remote = json!({"Id": user, "Name":"alice","ServerId":"backend",
            "Policy":{"IsAdministrator":false,"SyncPlayAccess":"None"}});
        Mock::given(method("POST"))
            .and(path("/Users/AuthenticateByName"))
            .and(body_json(json!({"Username":"alice","Pw":"password"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "User":remote,"ServerId":"backend","AccessToken":token,
                "SessionInfo":{"UserId":user,"UserName":"alice","ServerId":"backend"}
            })))
            .expect(2)
            .mount(backend)
            .await;
        let authorized = || header_regex("Authorization", &format!("Token=\"{token}\""));
        // Only the backend token issued above can satisfy a library request.
        let views = app.views_path(&user);
        Mock::given(method("GET")).and(path(views.split('?').next().unwrap()))
            .and(authorized()).respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items":[{"Id":f.original(index).await,"Name":"Movies","Type":"CollectionFolder","CollectionType":"movies"}],
                "TotalRecordCount":1,"StartIndex":0
            }))).expect(2).mount(backend).await;
        if matches!(app, Kodi | MpvShim) {
            Mock::given(method("GET"))
                .and(path("/system/info"))
                .and(authorized())
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"ServerName":"Movies","Id":"backend","LocalAddress":"http://backend.test"})),
                )
                .mount(backend)
                .await;
        }
        for route in ["/Users/Me".to_owned(), format!("/Users/{user}")] {
            Mock::given(method("GET"))
                .and(path(route))
                .and(authorized())
                .respond_with(ResponseTemplate::new(200).set_body_json(&remote))
                .mount(backend)
                .await;
        }
    }
}

async fn shim_socket(f: &Fixture, client: &Client, authorization: &str) {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let url = format!("{}/socket", f.url.replacen("http://", "ws://", 1));
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Authorization", authorization.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let message: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(message["MessageType"], "ForceKeepAlive");
    let response = client.post(format!("{}/Sessions/Capabilities/Full", f.url))
        .header("Authorization", authorization)
        .json(&json!({"PlayableMediaTypes":["Video"],"SupportsMediaControl":true,"SupportedCommands":["Play"]}))
        .send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    socket.close(None).await.unwrap();
}

async fn login_lifecycle(app: App) {
    let f = Fixture::new().await;
    f.healthy().await;
    mount_backends(&f, app).await;
    let client = Client::new();
    let url = &f.url;
    let user = f.user.id.as_str();
    let server = json_ok(client.get(format!("{url}/System/Info/Public"))).await;
    let server_id = server["Id"].as_str().unwrap();
    uuid::Uuid::parse_str(server_id).expect("SDK clients require a UUID server ID");
    assert_eq!(server["ProductName"], "Jellyfin Server");
    assert!(server["Version"]
        .as_str()
        .is_some_and(|version| !version.is_empty()));
    if matches!(app, BundledWeb(_) | JellyfinWeb | Finamp | Roku) {
        assert!(json_ok(client.get(format!("{url}/Users/Public")))
            .await
            .is_array());
    }
    let mut tokens = Vec::new();
    for use_quick_connect in [false, true] {
        let device = if app == AndroidTv && !use_quick_connect {
            "username-derived-id"
        } else {
            "install-id"
        };
        let login_header = app.authorization(device, user, None);
        let result = if use_quick_connect {
            let qc_header = if app == Streamyfin {
                r#"MediaBrowser Client="Streamyfin", Device=Android, DeviceId="install-id", Version="1""#
            } else {
                &login_header
            };
            quick_connect(&f, &client, app, qc_header).await
        } else {
            let route = match app {
                Roku => "/Users/AuthenticateByName?format=json",
                BundledWeb(_) => "/Users/authenticatebyname",
                _ => "/Users/AuthenticateByName",
            };
            let name = if matches!(app, Kodi | MpvShim) {
                "username"
            } else {
                "Username"
            };
            json_ok(
                client
                    .post(format!("{url}{route}"))
                    .header("Authorization", login_header)
                    .json(&json!({(name):"alice","Pw":"password"})),
            )
            .await
        };
        assert_eq!(result["ServerId"], server_id);
        assert_eq!(result["User"]["Id"], user);
        assert_eq!(result["User"]["Name"], "alice");
        assert_eq!(result["User"]["ServerId"], server_id);
        assert_eq!(result["SessionInfo"]["UserId"], user);
        let token = result["AccessToken"].as_str().unwrap();
        assert!(
            !token.is_empty()
                && token != f.user.fixture_token
                && !token.starts_with("issued-token-")
        );
        tokens.push(token.to_owned());

        // Fresh HTTP client with just the saved bearer. Android TV changes its device ID here.
        let resumed = Client::new();
        let device = if app == AndroidTv {
            "user-uuid-derived-id"
        } else {
            device
        };
        let header = app.authorization(device, user, Some(token));
        if matches!(app, Kodi | MpvShim) {
            // Both Python connection managers validate a stored credential at this lowercase URL.
            let info = json_ok(
                resumed
                    .get(format!("{url}/system/info"))
                    .header("Authorization", &header),
            )
            .await;
            assert_eq!(info["Id"], server_id);
        }
        let who = if matches!(app, Roku | Kodi | MpvShim) {
            format!("/Users/{user}")
        } else {
            "/Users/Me".into()
        };
        let me = json_ok(
            resumed
                .get(format!("{url}{who}"))
                .header("Authorization", &header),
        )
        .await;
        assert_eq!(me["Id"], user);
        let views = json_ok(
            resumed
                .get(format!("{url}{}", app.views_path(user)))
                .header("Authorization", &header),
        )
        .await;
        assert!(!views["Items"].as_array().unwrap().is_empty());
        if app == MpvShim {
            shim_socket(&f, &resumed, &header).await;
        }
        let logout = resumed
            .post(format!("{url}/Sessions/Logout"))
            .header("Authorization", &header)
            .send()
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::NO_CONTENT);
        let revoked = resumed
            .get(format!("{url}/Users/Me"))
            .header("Authorization", &header)
            .send()
            .await
            .unwrap();
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
    }
    assert_ne!(tokens[0], tokens[1]);
}

contract_test!(upstream_jellyfin_web, login_lifecycle, JellyfinWeb);
contract_test!(
    bundled_jellyfin_web,
    login_lifecycle,
    BundledWeb("Jellyfin%20Web")
);
contract_test!(
    jellyfin_desktop,
    login_lifecycle,
    BundledWeb("Jellyfin%20Desktop")
);
contract_test!(
    jellyfin_android,
    login_lifecycle,
    BundledWeb("Jellyfin%20for%20Android")
);
contract_test!(jellyfin_ios, login_lifecycle, BundledWeb("Jellyfin%20iOS"));
contract_test!(android_tv_device_id_transition, login_lifecycle, AndroidTv);
contract_test!(findroid, login_lifecycle, Findroid);
contract_test!(streamyfin, login_lifecycle, Streamyfin);
contract_test!(swiftfin, login_lifecycle, Swiftfin);
contract_test!(finamp, login_lifecycle, Finamp);
contract_test!(roku, login_lifecycle, Roku);
contract_test!(kodi, login_lifecycle, Kodi);
contract_test!(mpv_shim, login_lifecycle, MpvShim);
