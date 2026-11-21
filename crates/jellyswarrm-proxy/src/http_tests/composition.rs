//! Contracts for the shared production router, not a reduced test-only route set.
use super::*;

#[tokio::test]
async fn production_authentication_wiring_logs_in_with_encrypted_mappings_and_reuses_stored_sessions(
) {
    use crate::domain::security::{EncryptedPassword, MappingEncryptionKey};
    use wiremock::{
        matchers::{body_json, header_regex, method, path},
        Mock, ResponseTemplate,
    };

    let f = Fixture::new().await;
    f.healthy().await;
    for server in f.state.server_storage.list_servers().await.unwrap() {
        f.state
            .user_authorization
            .add_server_mapping(
                &f.user.id,
                &server,
                "remote-alice",
                &"remote-password".into(),
                Some(&f.user.local_credential.mapping_key()),
            )
            .await
            .unwrap();
    }
    for (index, backend) in f.upstreams.iter().enumerate() {
        Mock::given(method("POST")).and(path("/Users/AuthenticateByName"))
            .and(body_json(json!({"Username":"remote-alice","Pw":"remote-password"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "User":{"Id":format!("remote-{index}"),"Name":"Remote alice","ServerId":"backend","Policy":{"IsAdministrator":false,"SyncPlayAccess":"None"}},
                "AccessToken":format!("fresh-token-{index}"), "ServerId":"backend",
                "SessionInfo":{"UserId":format!("remote-{index}"),"UserName":"Remote alice","ServerId":"backend"}
            }))).expect(1).mount(backend).await;
        Mock::given(method("GET"))
            .and(path("/Users/Me"))
            .and(header_regex(
                "authorization",
                &format!("Token=\\\"fresh-token-{index}\\\""),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Id":format!("remote-{index}"),"Name":"Remote alice","ServerId":"backend",
                "Policy":{"IsAdministrator":false,"SyncPlayAccess":"None"}
            })))
            .mount(backend)
            .await;
    }
    let login = f
        .request(
            reqwest::Method::POST,
            "/Users/AuthenticateByName",
            &f.user,
            "production-login",
        )
        .json(&json!({"Username":"alice","Pw":"password"}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let login: Value = login.json().await.unwrap();
    let token = login["AccessToken"].as_str().unwrap();
    assert_ne!(token, f.user.fixture_token);
    assert_eq!(login["User"]["Id"], f.user.id.as_str());
    assert_eq!(login["User"]["Name"], "alice");

    let config = f.state.config.read().await.clone();
    let mapping_key = MappingEncryptionKey::from_session_key(&config.session_key).unwrap();
    let mappings: Vec<(String, String)> = sqlx::query_as(
        "SELECT mapped_password, credential_format FROM server_mappings WHERE user_id = ?",
    )
    .bind(&f.user.id)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(mappings.len(), 2);
    for (ciphertext, format) in mappings {
        assert_eq!(format, "session_v1");
        assert_ne!(ciphertext, "remote-password");
        assert_eq!(
            mapping_key
                .decrypt(&EncryptedPassword::from_raw(ciphertext))
                .unwrap()
                .as_str(),
            "remote-password"
        );
    }
    let session_key =
        MappingEncryptionKey::for_authorization_sessions(&config.session_key).unwrap();
    let sessions: Vec<String> =
        sqlx::query_scalar("SELECT jellyfin_token FROM login_sessions WHERE user_id = ?")
            .bind(&f.user.id)
            .fetch_all(&f.pool)
            .await
            .unwrap();
    assert_eq!(sessions.len(), 2);
    for ciphertext in sessions {
        assert!(!ciphertext.starts_with("fresh-token-"));
        assert!(session_key
            .decrypt(&EncryptedPassword::from_raw(ciphertext))
            .unwrap()
            .as_str()
            .starts_with("fresh-token-"));
    }
    // Reconstructing through the production seam also reads/decrypts the
    // persisted session rows, rather than relying on transient login objects.
    let rebuilt = crate::auth::UserAuthorizationService::from_config(f.pool.clone(), &config)
        .await
        .unwrap();
    let (_, credential) = rebuilt.resolve_credential(token).await.unwrap().unwrap();
    let sessions = rebuilt
        .sessions_for_credential(&f.user.id, credential)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(sessions
        .iter()
        .all(|(session, _)| session.jellyfin_token.starts_with("fresh-token-")));

    // Ordinary authenticated HTTP now uses the stored backend token. The login
    // mocks' exact counts ensure that this request never re-authenticates.
    let me = reqwest::Client::new()
        .get(format!("{}/Users/Me", f.url))
        .header("X-Emby-Token", token)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), StatusCode::OK);
    let me: Value = me.json().await.unwrap();
    assert_eq!(me["Id"], f.user.id.as_str());
    assert_eq!(me["Name"], "alice");
    for backend in &f.upstreams {
        backend.verify().await;
    }
}

#[tokio::test]
async fn production_composition_mounts_root_probes_and_prefixed_feature_routes() {
    let f = Fixture::new().await;
    let root = f.url.strip_suffix("/proxy").unwrap();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for (path, body) in [("/health", "healthy"), ("/ready", "ready")] {
        let response = client.get(format!("{root}{path}")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), body);
    }
    // This route was absent from the old hand-built HTTP fixture.
    let enabled = client
        .get(format!("{}/QuickConnect/Enabled", f.url))
        .send()
        .await
        .unwrap();
    assert_eq!(enabled.status(), StatusCode::OK);
    assert!(enabled.json::<bool>().await.unwrap());

    let redirect = client
        .get(format!("{root}/QuickConnect/Enabled"))
        .send()
        .await
        .unwrap();
    assert_eq!(redirect.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        redirect.headers()["location"],
        "/proxy/QuickConnect/Enabled"
    );
    f.no_upstream_requests().await;
}

#[tokio::test]
async fn concurrent_tv_logins_route_and_revoke_independently() {
    use crate::auth::service::CallerCredential;
    use wiremock::{
        matchers::{header_regex, method, path},
        Mock, ResponseTemplate,
    };

    let f = Fixture::new().await;
    f.healthy().await;
    for (index, upstream) in f.upstreams.iter().enumerate() {
        for version in ["1", "2", "key"] {
            let backend_token = format!("backend-{index}-login-{version}");
            Mock::given(method("POST")).and(path("/Users/AuthenticateByName"))
                .and(header_regex("Authorization", &if version=="key" {String::from("jellyswarrm-key-")}else{format!(r#"Client="TV".*Version="{version}""#)}))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "User":{"Id":format!("remote-{index}"),"Name":"alice","ServerId":"backend","Policy":{"IsAdministrator":false,"SyncPlayAccess":"None"}},
                    "AccessToken":backend_token,"ServerId":"backend","SessionInfo":{"UserId":format!("remote-{index}"),"UserName":"alice","ServerId":"backend"}
                }))).expect(1).mount(upstream).await;
            let original = f.original(index).await;
            let expected = backend_token.clone();
            Mock::given(method("GET"))
                .and(path(format!("/Videos/{original}/stream")))
                .and(move |req: &wiremock::Request| {
                    req.url
                        .query_pairs()
                        .any(|(k, v)| k.eq_ignore_ascii_case("api_key") && v == expected)
                        || req
                            .headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .is_some_and(|v| v.contains(&expected))
                })
                .respond_with(ResponseTemplate::new(200).set_body_string(backend_token))
                .mount(upstream)
                .await;
        }
    }
    let client = reqwest::Client::new();
    let login = |version| {
        let request = client.post(format!("{}/Users/AuthenticateByName", f.url))
            .header("Authorization", format!("MediaBrowser Client=TV, Device=LivingRoom, DeviceId=tv-{version}, Version={version}"))
            .json(&json!({"Username":"alice","Pw":"password"}));
        async move {
            let response = request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            response["AccessToken"].as_str().unwrap().to_owned()
        }
    };
    let (first, second) = tokio::join!(login("1"), login("2"));
    assert_ne!(first, second);

    // Reopen the authorization service with shared tokens disabled.
    let mut config = f.state.config.read().await.clone();

    config.preserve_upstream_device_ids = true;
    let service = crate::auth::UserAuthorizationService::from_config(f.pool.clone(), &config)
        .await
        .unwrap();
    assert!(service
        .resolve_credential(
            &sqlx::query_scalar::<_, String>("SELECT virtual_key FROM users WHERE id=?")
                .bind(&f.user.id)
                .fetch_one(&f.pool)
                .await
                .unwrap()
        )
        .await
        .unwrap()
        .is_none());
    let allowed_device = Authorization::parse(&auth(&f.user, "allowlisted")).unwrap();
    assert_eq!(
        service.upstream_login_device(&allowed_device).device_id,
        "allowlisted"
    );
    for (version, token) in [("1", &first), ("2", &second)] {
        let (owner, credential) = service.resolve_credential(token).await.unwrap().unwrap();
        assert!(matches!(credential, CallerCredential::Login(_)));
        assert_eq!(owner.id, f.user.id);
        assert_eq!(
            service
                .sessions_for_credential(&owner.id, credential)
                .await
                .unwrap()
                .len(),
            2
        );
        for (index, item) in f.items.iter().enumerate() {
            let stream = client.get(format!("{}/Videos/{item}/stream?Static=true", f.url));
            let stream = if version == "1" {
                stream.header("Authorization", format!("MediaBrowser Client=Renamed, Device=LivingRoom, DeviceId=rotated-id, Version=3, Token={token}"))
            } else {
                stream.query(&[("api_key", token)])
            };
            let response = stream.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.text().await.unwrap(),
                format!("backend-{index}-login-{version}")
            );
        }
    }
    let primary = client
        .post(format!("{}/Auth/Keys?App=Scanner", f.url))
        .header("X-Emby-Token", &first)
        .send()
        .await
        .unwrap();
    assert_eq!(primary.status(), StatusCode::NO_CONTENT);
    let scanner = service
        .list_api_keys(&f.user.id)
        .await
        .unwrap()
        .pop()
        .unwrap()
        .access_token;
    let (_, CallerCredential::Login(first_id)) =
        service.resolve_credential(&first).await.unwrap().unwrap()
    else {
        panic!("expected client login")
    };
    sqlx::query("UPDATE login_sessions SET expires_at = '2000-01-01' WHERE login_id = ?")
        .bind(first_id)
        .execute(&f.pool)
        .await
        .unwrap();
    let unavailable = client
        .get(format!("{}/Videos/{}/stream", f.url, f.items[0]))
        .query(&[("api_key", &first)])
        .send()
        .await
        .unwrap();
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    use futures_util::StreamExt;
    let socket_url = format!(
        "{}/socket?api_key={first}&deviceId=cast",
        f.url.replacen("http", "ws", 1)
    );
    let (mut cast, _) = tokio_tungstenite::connect_async(&socket_url).await.unwrap();
    cast.next().await.unwrap().unwrap();
    let logout = client
        .post(format!("{}/Sessions/Logout", f.url))
        .header("X-Emby-Token", &first)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    assert!(service.resolve_credential(&first).await.unwrap().is_none());
    assert!(tokio_tungstenite::connect_async(&socket_url).await.is_err());
    assert!(f
        .state
        .client_sessions
        .for_user(&f.user.id)
        .await
        .iter()
        .all(|player| player.login_id != Some(first_id)));
    // The scanner has independent read-only access after its issuer signs out.
    for (token, expected) in [
        (&second, "backend-0-login-2"),
        (&scanner, "backend-0-login-key"),
    ] {
        let response = client
            .get(format!(
                "{}/Videos/{}/stream?Static=true",
                f.url, f.items[0]
            ))
            .query(&[("api_key", token)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), expected);
    }
    let denied = client
        .post(format!("{}/Auth/Keys?App=Nested", f.url))
        .header(
            "Authorization",
            "MediaBrowser Client=Scanner, Device=Scanner, DeviceId=scan, Version=1",
        )
        .header("X-Emby-Token", scanner)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn mapping_relinked_during_login_skips_only_that_backend() {
    use crate::auth::mapping_authentication::BackendLogin;
    use wiremock::{
        matchers::{method, path},
        Mock, ResponseTemplate,
    };
    let f = Fixture::new().await;
    let service = &f.state.user_authorization;
    let mut backends = Vec::new();
    for server in f.state.server_storage.list_servers().await.unwrap() {
        let mapping = service
            .get_server_mapping_by_server_id(&f.user.id, server.id)
            .await
            .unwrap()
            .unwrap();
        let backend = f
            .upstreams
            .iter()
            .find(|b| b.uri() == server.url.as_str())
            .unwrap();
        Mock::given(method("POST")).and(path("/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "User":{"Id":"remote","Name":"alice","ServerId":"backend","Policy":{"IsAdministrator":false,"SyncPlayAccess":"None"}},
                "AccessToken":"pending-login","ServerId":"backend","SessionInfo":{"UserId":"remote","UserName":"alice","ServerId":"backend"}
            }))).mount(backend).await;
        let login = f
            .state
            .mapping_authenticator
            .authenticate_password(
                &server,
                "alice",
                &"password".into(),
                crate::config::CLIENT_INFO.clone(),
            )
            .await
            .unwrap();
        backends.push(BackendLogin {
            server,
            mapping: Some(mapping),
            login,
        });
    }
    service
        .add_server_mapping(
            &f.user.id,
            &backends[1].server,
            "new-owner",
            &"new-password".into(),
            Some(&f.user.local_credential.mapping_key()),
        )
        .await
        .unwrap();
    let device = Authorization::parse(&auth(&f.user, "tv")).unwrap();
    let (token, first, _) = service
        .create_login(&f.user, &device, "upstream-id", &backends)
        .await
        .unwrap();
    assert_eq!(first, 0);
    let (_, credential) = service.resolve_credential(&token).await.unwrap().unwrap();
    assert_eq!(
        service
            .sessions_for_credential(&f.user.id, credential)
            .await
            .unwrap()
            .len(),
        1
    );
    // Replaying a stale set must leave the successfully published login usable.
    assert!(matches!(
        service
            .create_login(&f.user, &device, "retry", &backends)
            .await,
        Err(sqlx::Error::RowNotFound)
    ));
    assert!(service.resolve_credential(&token).await.unwrap().is_some());

    let rejected_url = backends[1].server.url.clone();
    drop(backends);
    tokio::time::timeout(std::time::Duration::from_secs(2),async{
        loop{
            let ready:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pending_upstream_logouts WHERE server_url=? AND next_attempt=0)")
                .bind(rejected_url.as_str()).fetch_one(&f.pool).await.unwrap();
            if ready{break}tokio::task::yield_now().await;
        }
    }).await.unwrap();
    // A timeout or server error retains the encrypted entry for retry.
    f.state.mapping_authenticator.drain_logouts().await.unwrap();
    let attempts: i64 = sqlx::query_scalar(
        "SELECT attempts FROM pending_upstream_logouts WHERE server_url=? AND attempts>0",
    )
    .bind(rejected_url.as_str())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn placeholder_logins_coexist_and_an_unbound_key_signs_in_on_first_use() {
    use wiremock::{
        matchers::{method, path},
        Mock, ResponseTemplate,
    };
    let f = Fixture::new().await;
    f.healthy().await;
    for upstream in &f.upstreams {
        Mock::given(method("POST")).and(path("/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "User":{"Id":"remote","Name":"alice","ServerId":"backend","Policy":{"IsAdministrator":false,"SyncPlayAccess":"None"}},
                "AccessToken":"mock-password-token","ServerId":"backend","SessionInfo":{"UserId":"remote","UserName":"alice","ServerId":"backend"}
            }))).expect(3).mount(upstream).await;
    }
    let auth = "MediaBrowser Client=Player, Device=Unknown, DeviceId=unknown-device-id, Version=1";
    let mut tokens = Vec::new();
    for _ in 0..2 {
        let response = reqwest::Client::new()
            .post(format!("{}/Users/AuthenticateByName", f.url))
            .header("Authorization", auth)
            .json(&json!({"Username":"alice","Pw":"password"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        tokens.push(response["AccessToken"].as_str().unwrap().to_owned());
    }
    for token in tokens {
        assert!(f
            .state
            .user_authorization
            .resolve_credential(&token)
            .await
            .unwrap()
            .is_some());
    }
    let cfg = crate::config::AppConfig {
        preserve_upstream_device_ids: true,
        ..f.state.config.read().await.clone()
    };
    let service = crate::auth::UserAuthorizationService::from_config(f.pool.clone(), &cfg)
        .await
        .unwrap();
    let auth = Authorization::parse(auth).unwrap();
    assert_ne!(
        service.upstream_login_device(&auth).device_id,
        service.upstream_login_device(&auth).device_id
    );

    let (_, login_id) = service
        .create_api_key(&f.user, "migrated-scanner")
        .await
        .unwrap();
    let key_login = crate::auth::service::CallerCredential::ApplicationKey(login_id);
    assert!(service
        .sessions_for_credential(&f.user.id, key_login)
        .await
        .unwrap()
        .is_empty());
    let hydrate = || {
        f.state
            .mapping_authenticator
            .ensure_key_sessions(&f.user, login_id)
    };
    tokio::try_join!(hydrate(), hydrate()).unwrap();
    assert_eq!(
        service
            .sessions_for_credential(&f.user.id, key_login)
            .await
            .unwrap()
            .len(),
        2
    );
    for backend in &f.upstreams {
        backend.verify().await;
    }
}

#[tokio::test]
async fn admin_revocation_disconnects_a_player_without_another_client_request() {
    use futures_util::StreamExt;
    let f = Fixture::new().await;
    let mut socket = f.socket("tv").await;
    let operations = f.state.sessions.clone();
    let task = tokio::spawn(async move { operations.maintenance_loop(0).await });
    f.state
        .user_authorization
        .delete_all_sessions_for_user(&f.user.id)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(Ok(message)) = socket.next().await {
            if message.is_close() {
                break;
            }
        }
    })
    .await
    .expect("revoked socket stayed open");
    task.abort();
    assert!(f
        .state
        .client_sessions
        .for_user(&f.user.id)
        .await
        .is_empty());
}
