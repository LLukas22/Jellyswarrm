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
    assert_ne!(token, f.user.virtual_key);
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
    let sessions: Vec<(String, String)> = sqlx::query_as("SELECT jellyfin_token, token_format FROM authorization_sessions WHERE user_id = ? AND login_id IS NOT NULL")
        .bind(&f.user.id).fetch_all(&f.pool).await.unwrap();
    assert_eq!(sessions.len(), 2);
    for (ciphertext, format) in sessions {
        assert_eq!(format, "session_v1");
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
        .sessions_for_credential(&f.user.id, None, credential)
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
        for version in ["1", "2"] {
            let backend_token = format!("backend-{index}-login-{version}");
            Mock::given(method("POST")).and(path("/Users/AuthenticateByName"))
                .and(header_regex("Authorization", &format!(r#"Version="{version}""#)))
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
            .header("Authorization", format!("MediaBrowser Client=TV, Device=LivingRoom, DeviceId=same-id, Version={version}"))
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
    config.disable_legacy_login_tokens = true;
    config.preserve_upstream_device_ids = true;
    let service = crate::auth::UserAuthorizationService::from_config(f.pool.clone(), &config)
        .await
        .unwrap();
    assert!(service
        .resolve_credential(&f.user.virtual_key)
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
                .sessions_for_credential(&owner.id, None, credential)
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
    sqlx::query("UPDATE authorization_sessions SET expires_at = '2000-01-01' WHERE login_id = ?")
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
        .all(|player| player.token() != first));
    // The scanner has independent read-only access after its issuer signs out.
    for (token, expected) in [
        (&second, "backend-0-login-2"),
        (&scanner, "backend-0-login-1"),
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
async fn mapping_relinked_during_login_rolls_back_all_bindings() {
    use crate::auth::mapping_authentication::{BackendLogin, MappingLogin};
    let f = Fixture::new().await;
    let service = &f.state.user_authorization;
    let mut backends = Vec::new();
    for server in f.state.server_storage.list_servers().await.unwrap() {
        let mapping = service
            .get_server_mapping_by_server_id(&f.user.id, server.id)
            .await
            .unwrap()
            .unwrap();
        backends.push(BackendLogin {
            server,
            mapping: Some(mapping),
            login: MappingLogin {
                response: serde_json::from_value(json!({
                    "User":{"Id":"remote","Name":"alice","ServerId":"backend","Policy":{"IsAdministrator":false,"SyncPlayAccess":"None"}},
                    "AccessToken":"old-login","ServerId":"backend","SessionInfo":{"UserId":"remote","UserName":"alice","ServerId":"backend"}
                })).unwrap(),
                username:"alice".into(), password:Some("password".into()),
            }
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
    assert!(service
        .create_login(&f.user, &device, "upstream-id", &backends)
        .await
        .is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM client_logins")
            .fetch_one(&f.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM authorization_sessions WHERE login_id IS NOT NULL"
        )
        .fetch_one(&f.pool)
        .await
        .unwrap(),
        0
    );
}
