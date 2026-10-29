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
    assert_eq!(login["AccessToken"], f.user.virtual_key);
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
    let sessions: Vec<(String, String)> = sqlx::query_as("SELECT jellyfin_token, token_format FROM authorization_sessions WHERE user_id = ? AND device_id = 'production-login'")
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
    let rebuilt = crate::app::authorization::build_authorization(f.pool.clone(), &config)
        .await
        .unwrap();
    let sessions = rebuilt
        .get_user_sessions(&f.user.id, Some(super::support::device("production-login")))
        .await
        .unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(sessions
        .iter()
        .all(|(session, _)| session.jellyfin_token.starts_with("fresh-token-")));

    // Ordinary authenticated HTTP now uses the stored backend token. The login
    // mocks' exact counts ensure that this request never re-authenticates.
    let me = f
        .request(
            reqwest::Method::GET,
            "/Users/Me",
            &f.user,
            "production-login",
        )
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
