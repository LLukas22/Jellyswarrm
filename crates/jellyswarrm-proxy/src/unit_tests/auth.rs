//! Regression coverage for credentials, account mappings, and authorization-session invariants.
use crate::persistence::MIGRATOR;

use super::*;

async fn setup_service() -> (SqlitePool, UserAuthorizationService) {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let service = UserAuthorizationService::new(pool.clone());
    (pool, service)
}

#[tokio::test]
async fn user_lookups_accept_equivalent_uuid_formats() {
    let (_, service) = setup_service().await;
    let user = service
        .create_user("viewer", &"password".into())
        .await
        .unwrap();
    let uuid = uuid::Uuid::parse_str(&user.id).unwrap();
    for id in [
        uuid.simple().to_string(),
        uuid.hyphenated().to_string(),
        uuid.simple().to_string().to_uppercase(),
        uuid.hyphenated().to_string().to_uppercase(),
    ] {
        assert_eq!(
            service.get_user_by_id(&id).await.unwrap().unwrap().id,
            user.id
        );
        let (resolved, sessions) = service
            .get_user_sessions_by_user_id(&id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.id, user.id);
        assert!(sessions.is_empty());
    }
    for unknown in [
        "unknown".to_string(),
        uuid::Uuid::new_v4().hyphenated().to_string(),
    ] {
        assert!(service.get_user_by_id(&unknown).await.unwrap().is_none());
        assert!(service
            .get_user_sessions_by_user_id(&unknown)
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn delayed_password_change_cannot_overwrite_newer_credentials_or_mappings() {
    let (pool, service) = setup_service().await;
    let server_id = insert_test_server(&pool, "Backend", "http://localhost:8096").await;
    let user = service
        .create_user("local", &"before".into())
        .await
        .unwrap();
    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "remote",
            &"backend-secret".into(),
            Some(&user.local_credential.mapping_key()),
        )
        .await
        .unwrap();

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let delayed_service = service.clone();
    let user_id = user.id.clone();
    let delayed = tokio::spawn(async move {
        let snapshot = delayed_service
            .get_user_by_id(&user_id)
            .await
            .unwrap()
            .unwrap();
        assert!(snapshot
            .local_credential
            .verify_async(&"before".into())
            .await
            .unwrap());
        let replacement = LocalCredential::from_password(&"delayed".into())
            .await
            .unwrap();
        ready_tx.send(()).unwrap();
        resume_rx.await.unwrap();
        delayed_service
            .update_user_password_if_current(
                &snapshot,
                &"before".into(),
                &replacement,
                &"admin".into(),
            )
            .await
            .unwrap()
    });

    // Both requests verified the same old credential. Let the second
    // request commit while the first is paused before its transaction.
    ready_rx.await.unwrap();
    assert!(service
        .update_user_password(&user.id, &"before".into(), &"newer".into(), &"admin".into())
        .await
        .unwrap());
    let winner = service.get_user_by_id(&user.id).await.unwrap().unwrap();
    let saved_mapping: String =
        sqlx::query_scalar("SELECT mapped_password FROM server_mappings WHERE user_id = ?")
            .bind(&user.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    resume_tx.send(()).unwrap();
    assert!(!delayed.await.unwrap());
    assert_eq!(
        service
            .get_user_by_id(&user.id)
            .await
            .unwrap()
            .unwrap()
            .local_credential,
        winner.local_credential
    );
    let mapping_after: String =
        sqlx::query_scalar("SELECT mapped_password FROM server_mappings WHERE user_id = ?")
            .bind(&user.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(mapping_after, saved_mapping);
    let mapping = service
        .get_server_mapping_by_server_id(&user.id, server_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        service
            .decrypt_server_mapping_password(
                &mapping,
                &winner.local_credential.mapping_key(),
                &HashedPassword::from_password("admin"),
                None,
                None
            )
            .unwrap()
            .as_str(),
        "backend-secret"
    );

    // Also reject a request whose earlier UI verification became stale
    // during backend validation, before entering the service at all.
    assert!(!service
        .update_user_password(&user.id, &"before".into(), &"stale".into(), &"admin".into())
        .await
        .unwrap());
    assert!(service
        .verify_user_password(&user.id, &"newer".into())
        .await
        .unwrap());
    assert!(!service
        .verify_user_password(&user.id, &"delayed".into())
        .await
        .unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn password_work_runs_off_runtime_and_verifiers_use_distinct_salts() {
    let runtime_thread = std::thread::current().id();
    let worker_thread = password_work(|| std::thread::current().id()).await.unwrap();
    assert_ne!(runtime_thread, worker_thread);
    let first = LocalCredential::from_password(&"password".into())
        .await
        .unwrap();
    let second = LocalCredential::from_password(&"password".into())
        .await
        .unwrap();
    assert_ne!(first, second);
    assert!(first.verify_async(&"password".into()).await.unwrap());
    assert!(!first.verify_async(&"wrong".into()).await.unwrap());
}

#[tokio::test]
async fn ui_login_migrates_raw_password_key_mapping_before_replacing_sha256() {
    let (pool, legacy) = setup_service().await;
    let id = insert_test_server(&pool, "Backend", "http://localhost:8096").await;
    let server = Server::from_row(
        sqlx::query("SELECT * FROM servers WHERE id = ?")
            .bind(id.as_i64())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    let user = legacy
        .create_user("local", &"password".into())
        .await
        .unwrap();
    let legacy_hash = HashedPassword::from_password("password");
    sqlx::query("UPDATE users SET original_password_hash = ?, local_credential_kind = 'password' WHERE id = ?")
            .bind(&legacy_hash).bind(&user.id).execute(&pool).await.unwrap();
    // Simulate the historical AES key derived from the raw password rather
    // than from its SHA-256 verifier.
    let raw_key = HashedPassword::from_hashed("password".into());
    let mapping_id = legacy
        .add_server_mapping(
            &user.id,
            &server,
            "remote",
            &"backend-password".into(),
            Some(&raw_key),
        )
        .await
        .unwrap();
    let protected = UserAuthorizationService::with_mapping_key(
        pool,
        MappingEncryptionKey::from_session_key(&[7; 64]).unwrap(),
        HashedPassword::from_password("admin"),
    );
    assert!(protected
        .get_user_by_credentials("local", &"wrong".into())
        .await
        .unwrap()
        .is_none());
    let before = protected
        .get_server_mapping(&user.id, &server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.credential_format, CredentialFormat::Legacy);
    let upgraded = protected
        .get_user_by_credentials("local", &"password".into())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        upgraded.local_credential,
        LocalCredential::Argon2(_)
    ));
    let mapping = protected
        .get_server_mapping(&user.id, &server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mapping.id, mapping_id);
    assert_eq!(mapping.credential_format, CredentialFormat::SessionV1);
    assert_eq!(
        protected
            .decrypt_server_mapping_password(
                &mapping,
                &upgraded.local_credential.mapping_key(),
                &legacy_hash,
                None,
                None
            )
            .unwrap()
            .as_str(),
        "backend-password"
    );
    assert!(protected
        .get_user_by_credentials("local", &"password".into())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn session_tokens_are_encrypted_on_upgrade_and_new_writes() {
    let (pool, legacy) = setup_service().await;
    let id = insert_test_server(&pool, "Backend", "http://localhost:8096").await;
    let server = Server::from_row(
        sqlx::query("SELECT * FROM servers WHERE id = ?")
            .bind(id.as_i64())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    let user = legacy
        .create_user("local", &"password".into())
        .await
        .unwrap();
    legacy
        .add_server_mapping(
            &user.id,
            &server,
            "remote",
            &"secret".into(),
            Some(&user.local_credential.mapping_key()),
        )
        .await
        .unwrap();
    let auth = Authorization {
        client: "test".into(),
        device: "test".into(),
        device_id: "device".into(),
        version: "1".into(),
        token: None,
    };
    legacy
        .store_authorization_session(
            &user.id,
            &server,
            &auth,
            "persistent-backend-token".into(),
            "remote-id".into(),
            None,
        )
        .await
        .unwrap();

    let mut protected = UserAuthorizationService::with_mapping_key(
        pool.clone(),
        MappingEncryptionKey::from_session_key(&[7; 64]).unwrap(),
        HashedPassword::from_password("admin"),
    );
    protected.enable_session_encryption(&[7; 64]).await.unwrap();
    let stored: (String, String) = sqlx::query_as(
            "SELECT jellyfin_token, token_format FROM authorization_sessions WHERE device_id = 'device'")
            .fetch_one(&pool).await.unwrap();
    assert_eq!(stored.1, "session_v1");
    assert!(!stored.0.contains("persistent-backend-token"));
    assert_eq!(
        protected.get_user_sessions(&user.id, None).await.unwrap()[0]
            .0
            .jellyfin_token,
        "persistent-backend-token"
    );
    protected
        .store_authorization_session(
            &user.id,
            &server,
            &auth,
            "replacement-token".into(),
            "remote-id".into(),
            None,
        )
        .await
        .unwrap();
    let stored: String = sqlx::query_scalar(
        "SELECT jellyfin_token FROM authorization_sessions WHERE device_id = 'device'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!stored.contains("replacement-token"));
    protected.enable_session_encryption(&[7; 64]).await.unwrap();
    let after_restart: String = sqlx::query_scalar(
        "SELECT jellyfin_token FROM authorization_sessions WHERE device_id = 'device'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, after_restart);
    assert_eq!(
        protected.get_user_sessions(&user.id, None).await.unwrap()[0]
            .0
            .jellyfin_token,
        "replacement-token"
    );
}

#[tokio::test]
async fn legacy_login_hash_upgrades_after_mapping_is_rekeyed() {
    let (pool, legacy) = setup_service().await;
    let id = insert_test_server(&pool, "Backend", "http://localhost:8096").await;
    let server = Server::from_row(
        sqlx::query("SELECT * FROM servers WHERE id = ?")
            .bind(id.as_i64())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    let user = legacy
        .create_user("local", &"password".into())
        .await
        .unwrap();
    let legacy_hash = HashedPassword::from_password("password");
    sqlx::query("UPDATE users SET original_password_hash = ?, local_credential_kind = 'password' WHERE id = ?")
            .bind(&legacy_hash).bind(&user.id).execute(&pool).await.unwrap();
    legacy
        .add_server_mapping(
            &user.id,
            &server,
            "remote",
            &"backend-password".into(),
            Some(&legacy_hash),
        )
        .await
        .unwrap();
    let protected = UserAuthorizationService::with_mapping_key(
        pool,
        MappingEncryptionKey::from_session_key(&[7; 64]).unwrap(),
        HashedPassword::from_password("admin"),
    );
    let upgraded = protected
        .get_user_by_credentials("local", &"password".into())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        upgraded.local_credential,
        LocalCredential::Argon2(_)
    ));
    let mapping = protected
        .get_server_mapping(&user.id, &server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mapping.credential_format, CredentialFormat::SessionV1);
    assert_eq!(
        protected
            .decrypt_server_mapping_password(
                &mapping,
                &upgraded.local_credential.mapping_key(),
                &legacy_hash,
                None,
                None
            )
            .unwrap()
            .as_str(),
        "backend-password"
    );
}

#[tokio::test]
async fn ambiguous_base64_legacy_plaintext_is_not_reencrypted_on_read() {
    let (pool, legacy) = setup_service().await;
    let id = insert_test_server(&pool, "Backend", "http://localhost:8096").await;
    let server = Server::from_row(
        sqlx::query("SELECT * FROM servers WHERE id = ?")
            .bind(id.as_i64())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    let user = legacy
        .create_user("local", &"password".into())
        .await
        .unwrap();
    let value = "ABCDEFGHIJKLMNOPQRSTabcdefghijklmnopqrst";
    let mapping_id = legacy
        .add_server_mapping(&user.id, &server, "remote", &value.into(), None)
        .await
        .unwrap();
    assert!(!is_legacy_plaintext(&EncryptedPassword::from_raw(
        value.into()
    )));
    let protected = UserAuthorizationService::with_mapping_key(
        pool,
        MappingEncryptionKey::from_session_key(&[7; 64]).unwrap(),
        HashedPassword::from_password("admin"),
    );
    let mapping = protected
        .get_server_mapping(&user.id, &server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mapping.id, mapping_id);
    assert_eq!(mapping.credential_format, CredentialFormat::Legacy);
    assert_eq!(
        protected
            .decrypt_server_mapping_password(
                &mapping,
                &user.local_credential.mapping_key(),
                &HashedPassword::from_password("admin"),
                None,
                None
            )
            .unwrap()
            .as_str(),
        value
    );
}

#[tokio::test]
async fn legacy_password_mapping_upgrades_without_changing_mapping_or_sessions() {
    let (pool, legacy) = setup_service().await;
    let id = insert_test_server(&pool, "Backend", "http://localhost:8096").await;
    let server = Server::from_row(
        sqlx::query("SELECT * FROM servers WHERE id = ?")
            .bind(id.as_i64())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    let user = legacy
        .create_user("local", &"login-password".into())
        .await
        .unwrap();
    let mapping_id = legacy
        .add_server_mapping(
            &user.id,
            &server,
            "remote",
            &"backend-password".into(),
            Some(&user.local_credential.mapping_key()),
        )
        .await
        .unwrap();
    let session = Authorization {
        client: "test".into(),
        device: "test".into(),
        device_id: "device".into(),
        version: "1".into(),
        token: None,
    };
    legacy
        .store_authorization_session(
            &user.id,
            &server,
            &session,
            "upstream-token".into(),
            "remote-id".into(),
            None,
        )
        .await
        .unwrap();

    let upgraded = UserAuthorizationService::with_mapping_key(
        pool.clone(),
        MappingEncryptionKey::from_session_key(&[3u8; 64]).unwrap(),
        HashedPassword::from_password("admin"),
    );
    let mapping = upgraded
        .get_server_mapping(&user.id, &server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mapping.id, mapping_id);
    assert_eq!(mapping.credential_format, CredentialFormat::SessionV1);
    assert_eq!(
        upgraded
            .decrypt_server_mapping_password(
                &mapping,
                &user.local_credential.mapping_key(),
                &HashedPassword::from_password("admin"),
                None,
                None
            )
            .unwrap()
            .as_str(),
        "backend-password"
    );
    assert_eq!(
        upgraded
            .get_user_sessions(&user.id, None)
            .await
            .unwrap()
            .len(),
        1
    );
    let wrong_key = UserAuthorizationService::with_mapping_key(
        pool.clone(),
        MappingEncryptionKey::from_session_key(&[9u8; 64]).unwrap(),
        HashedPassword::from_password("admin"),
    );
    let mapping_with_wrong_key = wrong_key
        .get_server_mapping(&user.id, &server)
        .await
        .unwrap()
        .unwrap();
    assert!(wrong_key
        .decrypt_server_mapping_password(
            &mapping_with_wrong_key,
            &user.local_credential.mapping_key(),
            &HashedPassword::from_password("admin"),
            None,
            None,
        )
        .is_err());
    // Re-saving identical credentials must preserve sessions and the format.
    upgraded
        .add_server_mapping(
            &user.id,
            &server,
            "remote",
            &"backend-password".into(),
            Some(&user.local_credential.mapping_key()),
        )
        .await
        .unwrap();
    assert_eq!(
        upgraded
            .get_user_sessions(&user.id, None)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn legacy_quick_connect_token_upgrades_and_survives_password_change() {
    let (pool, legacy) = setup_service().await;
    let id = insert_test_server(&pool, "Backend", "http://localhost:8096").await;
    let server = Server::from_row(
        sqlx::query("SELECT * FROM servers WHERE id = ?")
            .bind(id.as_i64())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    let user = legacy.create_user("local", &"before".into()).await.unwrap();
    let admin_key = HashedPassword::from_password("admin");
    legacy
        .add_quick_connect_mapping(
            &user.id,
            &server,
            "remote",
            "remote-id",
            "backend-token",
            &admin_key,
        )
        .await
        .unwrap();
    let upgraded = UserAuthorizationService::with_mapping_key(
        pool,
        MappingEncryptionKey::from_session_key(&[4u8; 64]).unwrap(),
        admin_key.clone(),
    );
    let mapping = upgraded
        .get_server_mapping(&user.id, &server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mapping.credential_format, CredentialFormat::SessionV1);
    upgraded
        .update_user_password(&user.id, &"before".into(), &"after".into(), &"admin".into())
        .await
        .unwrap();
    let mapping = upgraded
        .get_server_mapping(&user.id, &server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        upgraded
            .decrypt_mapping_token(
                &mapping,
                &HashedPassword::from_password("after"),
                &admin_key
            )
            .unwrap(),
        "backend-token"
    );
}

#[tokio::test]
async fn quick_connect_mapping_rekeys_and_switches_to_password() {
    let (pool, service) = setup_service().await;
    let server_id = insert_test_server(&pool, "Backend", "http://localhost:8096").await;
    let user = service.create_user("local", &"old".into()).await.unwrap();
    let server = Server::from_row(
        sqlx::query("SELECT * FROM servers WHERE id = ?")
            .bind(server_id.as_i64())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    service
        .add_quick_connect_mapping(
            &user.id,
            &server,
            "Remote",
            "backend-id",
            "bearer-secret",
            &user.local_credential.mapping_key(),
        )
        .await
        .unwrap();
    let mapping = service
        .get_server_mapping(&user.id, &server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&mapping.auth, MappingAuth::QuickConnect { backend_user_id, .. } if backend_user_id == "backend-id")
    );
    assert_eq!(
        service
            .decrypt_mapping_token(
                &mapping,
                &user.local_credential.mapping_key(),
                &HashedPassword::from_password("admin")
            )
            .unwrap(),
        "bearer-secret"
    );
    assert!(!mapping.auth.username().is_empty());

    service
        .update_user_password(&user.id, &"old".into(), &"new".into(), &"admin".into())
        .await
        .unwrap();
    let mapping = service
        .get_server_mapping(&user.id, &server)
        .await
        .unwrap()
        .unwrap();
    let new_key = service
        .get_user_by_id(&user.id)
        .await
        .unwrap()
        .unwrap()
        .local_credential
        .mapping_key();
    assert_eq!(
        service
            .decrypt_mapping_token(&mapping, &new_key, &HashedPassword::from_password("admin"))
            .unwrap(),
        "bearer-secret"
    );
    assert!(service
        .decrypt_mapping_token(
            &mapping,
            &user.local_credential.mapping_key(),
            &HashedPassword::from_password("admin")
        )
        .is_err());

    service
        .add_server_mapping(
            &user.id,
            &server,
            "Remote",
            &"password".into(),
            Some(&new_key),
        )
        .await
        .unwrap();
    let mapping = service
        .get_server_mapping(&user.id, &server)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(mapping.auth, MappingAuth::Password { .. }));
    let persisted: Option<String> =
        sqlx::query_scalar("SELECT encrypted_token FROM server_mappings WHERE id = ?")
            .bind(mapping.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(persisted.is_none());
}

#[tokio::test]
async fn admin_encrypted_quick_connect_mapping_survives_local_password_change() {
    let (pool, service) = setup_service().await;
    let server_id = insert_test_server(&pool, "Backend", "http://localhost:8096").await;
    let server = Server::from_row(
        sqlx::query("SELECT * FROM servers WHERE id = ?")
            .bind(server_id.as_i64())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    let user = service
        .create_user("local", &"before".into())
        .await
        .unwrap();
    let admin_key = HashedPassword::from_password("admin");
    service
        .add_quick_connect_mapping(
            &user.id,
            &server,
            "Remote",
            "remote-id",
            "token",
            &admin_key,
        )
        .await
        .unwrap();
    service
        .update_user_password(&user.id, &"before".into(), &"after".into(), &"admin".into())
        .await
        .unwrap();
    let mapping = service
        .get_server_mapping(&user.id, &server)
        .await
        .unwrap()
        .unwrap();
    let user_key = service
        .get_user_by_id(&user.id)
        .await
        .unwrap()
        .unwrap()
        .local_credential
        .mapping_key();
    assert_eq!(
        service
            .decrypt_mapping_token(&mapping, &user_key, &admin_key)
            .unwrap(),
        "token"
    );
}

#[tokio::test]
async fn named_api_key_resolves_its_owner() {
    let (_, service) = setup_service().await;
    let user = service
        .create_user("seerr-owner", &Password::from("password"))
        .await
        .unwrap();
    let key = service.create_api_key(&user.id, "Seerr").await.unwrap();

    let resolved = service
        .get_user_by_token(&key.access_token)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(resolved.id, user.id);
}

#[tokio::test]
async fn api_key_listing_is_scoped_and_creation_ordered() {
    let (_, service) = setup_service().await;
    let owner = service
        .create_user("owner", &Password::from("password"))
        .await
        .unwrap();
    let other = service
        .create_user("other", &Password::from("password"))
        .await
        .unwrap();
    let first = service.create_api_key(&owner.id, "Seerr").await.unwrap();
    let second = service.create_api_key(&owner.id, "Seerr").await.unwrap();
    service.create_api_key(&other.id, "Other").await.unwrap();

    let listed = service.list_api_keys(&owner.id).await.unwrap();

    assert_eq!(
        listed
            .iter()
            .map(|key| key.access_token.as_str())
            .collect::<Vec<_>>(),
        vec![first.access_token, second.access_token]
    );
}

#[tokio::test]
async fn deleting_user_revokes_named_api_keys() {
    let (_, service) = setup_service().await;
    let user = service
        .create_user("owner", &Password::from("password"))
        .await
        .unwrap();
    let key = service.create_api_key(&user.id, "Seerr").await.unwrap();
    service.delete_user(&user.id).await.unwrap();

    let resolved = service.get_user_by_token(&key.access_token).await.unwrap();

    assert!(resolved.is_none());
}

async fn insert_test_server(pool: &SqlitePool, name: &str, url: &str) -> ServerId {
    let now = chrono::Utc::now();
    let id = sqlx::query_scalar::<_, i64>(
        r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            RETURNING id
            "#,
    )
    .bind(name)
    .bind(url)
    .bind(100)
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await
    .unwrap();

    ServerId::new(id)
}

#[test]
fn test_device_from_useragent_parsing() {
    // Test Switchfin format
    let device = Device::from_useragent("Switchfin/0.7.4 (Linux)");
    assert_eq!(device.client, "Switchfin");
    assert_eq!(device.version, "0.7.4");
    assert_eq!(device.device, "Linux");

    // Test Jellyfin Web format
    let device = Device::from_useragent("Jellyfin Web/10.8.13");
    assert_eq!(device.client, "Jellyfin Web");
    assert_eq!(device.version, "10.8.13");
    assert_eq!(device.device, "Unknown");

    // Test browser format
    let device =
        Device::from_useragent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36");
    assert_eq!(device.client, "Mozilla");
    assert_eq!(device.version, "5.0");
    assert_eq!(device.device, "Windows");

    // Test mobile format
    let device = Device::from_useragent("Jellyfin Mobile/1.0.0 (iOS)");
    assert_eq!(device.client, "Jellyfin Mobile");
    assert_eq!(device.version, "1.0.0");
    assert_eq!(device.device, "iOS");

    // Test macOS Safari
    let device = Device::from_useragent(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15",
    );
    assert_eq!(device.client, "Mozilla");
    assert_eq!(device.version, "5.0");
    assert_eq!(device.device, "macOS");

    // Test Android Chrome
    let device =
        Device::from_useragent("Mozilla/5.0 (Linux; Android 11; SM-G991B) AppleWebKit/537.36");
    assert_eq!(device.client, "Mozilla");
    assert_eq!(device.version, "5.0");
    assert_eq!(device.device, "Android");

    // Test fallback for unknown format
    let device = Device::from_useragent("SomeUnknownClient");
    assert_eq!(device.client, "SomeUnknownClient");
    assert_eq!(device.version, "0.0.0");
    assert_eq!(device.device, "Unknown");
}

#[tokio::test]
async fn test_get_or_create_user_uses_stable_username_identity() {
    let (_, service) = setup_service().await;

    let first = service
        .get_or_create_user("testuser", &"password-1".into())
        .await
        .unwrap();

    let second = service
        .get_or_create_user(" TestUser ", &"password-2".into())
        .await
        .unwrap();

    assert_eq!(
        first.id, second.id,
        "user identity should be username-based"
    );
    assert!(second.local_credential.verify(&"password-1".into()));
    assert!(
        !second.local_credential.verify(&"password-2".into()),
        "resolving an existing user must not mutate its local credential"
    );

    let all_users = service.list_users().await.unwrap();
    assert_eq!(
        all_users.len(),
        1,
        "should not create duplicate local users"
    );
}

#[tokio::test]
async fn test_login_resolution_never_adopts_a_different_existing_credential() {
    let (_pool, service) = setup_service().await;
    let existing = service
        .create_user("existing", &"correct".into())
        .await
        .unwrap();

    assert!(service
        .resolve_or_create_login_user("existing", &"wrong".into())
        .await
        .unwrap()
        .is_none());

    let resolved = service
        .resolve_or_create_login_user("existing", &"correct".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.id, existing.id);
    assert!(resolved.local_credential.verify(&"correct".into()));
}

#[tokio::test]
async fn test_create_user_allows_empty_password() {
    let (_pool, service) = setup_service().await;

    let user = service
        .create_user("passwordless", &"".into())
        .await
        .unwrap();

    assert_eq!(user.local_credential, LocalCredential::Passwordless);
    assert!(service
        .verify_user_password(&user.id, &"".into())
        .await
        .unwrap());
    assert!(!service
        .verify_user_password(&user.id, &"not-empty".into())
        .await
        .unwrap());
}

#[tokio::test]
async fn test_add_server_mapping_allows_empty_password() {
    let (pool, service) = setup_service().await;
    let server_id = insert_test_server(&pool, "Test Server", "http://localhost:8096").await;
    let user = service
        .create_user("passwordless", &"".into())
        .await
        .unwrap();
    let master_password = user.local_credential.mapping_key();

    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"".into(),
            Some(&master_password),
        )
        .await
        .unwrap();

    let mapping = service
        .get_server_mapping_by_server_id(&user.id, server_id)
        .await
        .unwrap()
        .unwrap();
    let mapped_password = service.decrypt_server_mapping_password(
        &mapping,
        &master_password,
        &master_password,
        None,
        None,
    );

    assert_eq!(mapped_password.unwrap().as_str(), "");
}

#[tokio::test]
async fn test_password_updates_change_credential_mode_and_reencrypt_mappings() {
    let (pool, service) = setup_service().await;
    let server_id = insert_test_server(&pool, "Test Server", "http://localhost:8096").await;
    let user = service
        .create_user("transitioning", &"protected".into())
        .await
        .unwrap();
    let initial_key = user.local_credential.mapping_key();

    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"mapped-secret".into(),
            Some(&initial_key),
        )
        .await
        .unwrap();

    service
        .update_user_password(&user.id, &"protected".into(), &"".into(), &"admin".into())
        .await
        .unwrap();

    let passwordless = service.get_user_by_id(&user.id).await.unwrap().unwrap();
    assert_eq!(passwordless.local_credential, LocalCredential::Passwordless);
    assert!(passwordless.local_credential.verify(&"".into()));
    assert!(!passwordless.local_credential.verify(&"protected".into()));

    let mapping = service
        .get_server_mapping_by_server_id(&user.id, server_id)
        .await
        .unwrap()
        .unwrap();
    let passwordless_key = passwordless.local_credential.mapping_key();
    assert_eq!(
        service
            .decrypt_server_mapping_password(
                &mapping,
                &passwordless_key,
                &passwordless_key,
                None,
                None,
            )
            .unwrap()
            .as_str(),
        "mapped-secret"
    );

    service
        .update_user_password(
            &user.id,
            &"".into(),
            &"protected-again".into(),
            &"admin".into(),
        )
        .await
        .unwrap();

    let protected = service.get_user_by_id(&user.id).await.unwrap().unwrap();
    assert!(matches!(
        protected.local_credential,
        LocalCredential::Argon2(_)
    ));
    assert!(protected.local_credential.verify(&"protected-again".into()));

    let mapping = service
        .get_server_mapping_by_server_id(&user.id, server_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        service
            .decrypt_server_mapping_password(
                &mapping,
                &protected.local_credential.mapping_key(),
                &protected.local_credential.mapping_key(),
                None,
                None,
            )
            .unwrap()
            .as_str(),
        "mapped-secret"
    );
}

#[tokio::test]
async fn test_store_authorization_session_upserts_existing_device_session() {
    let (pool, service) = setup_service().await;
    insert_test_server(&pool, "Test Server", "http://localhost:8096").await;

    let user = service
        .get_or_create_user("testuser", &"testpass".into())
        .await
        .unwrap();
    let mapping_id = service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"mappedpass".into(),
            None,
        )
        .await
        .unwrap();

    let mut auth = Authorization {
        client: "Test Client".to_string(),
        device: "Test Device".to_string(),
        device_id: "test-device-id".to_string(),
        version: "1.0.0".to_string(),
        token: None,
    };

    let first_session_id = service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &auth,
            "old-token".to_string(),
            "old-original-user".to_string(),
            None,
        )
        .await
        .unwrap();
    let first_created_at = sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
        "SELECT created_at FROM authorization_sessions WHERE id = ?",
    )
    .bind(first_session_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    auth.version = "2.0.0".to_string();

    let second_session_id = service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &auth,
            "new-token".to_string(),
            "new-original-user".to_string(),
            None,
        )
        .await
        .unwrap();

    assert_eq!(first_session_id, second_session_id);

    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM authorization_sessions WHERE mapping_id = ?",
    )
    .bind(mapping_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    let row = sqlx::query(
        r#"
            SELECT jellyfin_token, original_user_id, version, created_at
            FROM authorization_sessions
            WHERE id = ?
            "#,
    )
    .bind(first_session_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(row.get::<String, _>("jellyfin_token"), "new-token");
    assert_eq!(
        row.get::<String, _>("original_user_id"),
        "new-original-user"
    );
    assert_eq!(row.get::<String, _>("version"), "2.0.0");
    assert_eq!(
        row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        first_created_at
    );
}

#[tokio::test]
async fn test_get_mapped_servers_returns_server_rows_in_priority_order() {
    let (pool, service) = setup_service().await;
    let low_priority_server = insert_test_server(&pool, "Low Priority", "http://low:8096").await;
    let high_priority_server = insert_test_server(&pool, "High Priority", "http://high:8096").await;

    sqlx::query("UPDATE servers SET priority = ? WHERE id = ?")
        .bind(10)
        .bind(low_priority_server.as_i64())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE servers SET priority = ? WHERE id = ?")
        .bind(200)
        .bind(high_priority_server.as_i64())
        .execute(&pool)
        .await
        .unwrap();

    let user = service
        .get_or_create_user("testuser", &"testpass".into())
        .await
        .unwrap();
    service
        .add_server_mapping(
            &user.id,
            "http://low:8096/",
            "lowuser",
            &"lowpass".into(),
            None,
        )
        .await
        .unwrap();
    service
        .add_server_mapping(
            &user.id,
            "http://high:8096/",
            "highuser",
            &"highpass".into(),
            None,
        )
        .await
        .unwrap();

    let servers = service.get_mapped_servers(&user.id).await.unwrap();

    assert_eq!(
        servers
            .iter()
            .map(|server| server.name.as_str())
            .collect::<Vec<_>>(),
        vec!["High Priority", "Low Priority"]
    );
    assert_eq!(servers[0].url.as_str(), "http://high:8096");
    assert_eq!(servers[1].url.as_str(), "http://low:8096");
}

#[tokio::test]
async fn test_device_session_fallback_matching() {
    let (pool, service) = setup_service().await;
    insert_test_server(&pool, "Test Server", "http://localhost:8096").await;

    // Create user
    let user = service
        .get_or_create_user("testuser", &"testpass".into())
        .await
        .unwrap();

    // Add server mapping
    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"mappedpass".into(),
            None,
        )
        .await
        .unwrap();

    // Store a session with specific device info
    let auth = Authorization {
        client: "Switchfin".to_string(),
        device: "Linux".to_string(),
        device_id: "1234567890abcdef-stored".to_string(),
        version: "0.7.4".to_string(),
        token: None,
    };

    service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &auth,
            "jellyfin-token".to_string(),
            "original-jellyfin-user-id".to_string(),
            None,
        )
        .await
        .unwrap();

    // Test 1: Exact match (device_id + client)
    let query_device1 = Device {
        client: "Switchfin".to_string(),
        device: "Linux".to_string(),
        device_id: "1234567890abcdef-stored".to_string(),
        version: "0.7.4".to_string(),
    };
    let sessions1 = service
        .get_user_sessions(&user.id, Some(query_device1))
        .await
        .unwrap();
    assert_eq!(sessions1.len(), 1, "Should find exact match");

    // Test 2: Strict match when known device ids share the same short prefix
    let query_device2 = Device {
        client: "Switchfin".to_string(),
        device: "Linux".to_string(),
        device_id: "1234567890abcdef-query".to_string(),
        version: "0.7.4".to_string(),
    };
    let sessions2 = service
        .get_user_sessions(&user.id, Some(query_device2))
        .await
        .unwrap();
    assert_eq!(
        sessions2.len(),
        1,
        "Should find strict match by short device id prefix"
    );

    // Test 3: Fallback to device name + client when device id is truly unknown
    let query_device3 = Device {
        client: "Switchfin".to_string(),
        device: "Linux".to_string(),
        device_id: "unknown".to_string(),
        version: "0.7.4".to_string(),
    };
    let sessions3 = service
        .get_user_sessions(&user.id, Some(query_device3))
        .await
        .unwrap();
    assert_eq!(
        sessions3.len(),
        1,
        "Should find fallback match by device name + client"
    );

    // Test 4: Different known device ids should not fallback by device name
    let query_device4 = Device {
        client: "Switchfin".to_string(),
        device: "Linux".to_string(),
        device_id: "different-device-id".to_string(),
        version: "0.7.4".to_string(),
    };
    let sessions4 = service
        .get_user_sessions(&user.id, Some(query_device4))
        .await
        .unwrap();
    assert_eq!(
        sessions4.len(),
        0,
        "Should not fallback when both device ids are known and different"
    );

    // Test 5: No match when client and version are different
    let query_device4 = Device {
        client: "DifferentClient".to_string(),
        device: "Linux".to_string(),
        device_id: "1234567890abcdef-stored".to_string(),
        version: "1.0.0".to_string(),
    };
    let sessions5 = service
        .get_user_sessions(&user.id, Some(query_device4))
        .await
        .unwrap();
    assert_eq!(
        sessions5.len(),
        0,
        "Should not find any match when client and version differ"
    );

    // Test 6: Two distinct web (browser) device ids sharing the same 16-char base64
    // prefix ("Mozilla/5.0 (") must NOT fall back to a prefix match - every browser's
    // device_id starts with that prefix, so this used to collide across all of a
    // user's web sessions and let requests reuse another session's stale token.
    let web_device_stored = Device {
        client: "Jellyfin Web".to_string(),
        device: "Chrome".to_string(),
        device_id: "TW96aWxsYS81LjAgKFdpbmRvd3MgTlQgMTAuMCkgQ2hyb21lLzE0Ny4wfDE3ODM4NTg5NzQ1Mjc1"
            .to_string(),
        version: "10.11.10".to_string(),
    };
    service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &Authorization {
                client: web_device_stored.client.clone(),
                device: web_device_stored.device.clone(),
                device_id: web_device_stored.device_id.clone(),
                version: web_device_stored.version.clone(),
                token: None,
            },
            "jellyfin-token-web".to_string(),
            "original-jellyfin-user-id".to_string(),
            None,
        )
        .await
        .unwrap();

    let web_device_query = Device {
        client: "Jellyfin Web".to_string(),
        device: "Chrome Android".to_string(),
        device_id:
            "TW96aWxsYS81LjAgKExpbnV4OyBBbmRyb2lkIDEwKSBDaHJvbWUvMTUwLjB8MTc4MzQ0MTUzNjM2NQ=="
                .to_string(),
        version: "10.11.10".to_string(),
    };
    let sessions6 = service
        .get_user_sessions(&user.id, Some(web_device_query))
        .await
        .unwrap();
    assert_eq!(
        sessions6.len(),
        0,
        "Distinct web device ids sharing the base64 'Mozilla/5.0 (' prefix must not match"
    );
}

#[tokio::test]
async fn test_android_tv_device_id_rebind_after_login_transition() {
    let (pool, service) = setup_service().await;
    insert_test_server(&pool, "Test Server", "http://localhost:8096").await;

    let user = service
        .get_or_create_user("androidtv", &"testpass".into())
        .await
        .unwrap();

    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"mappedpass".into(),
            None,
        )
        .await
        .unwrap();

    let stored_auth = Authorization {
        client: "Jellyfin Android TV".to_string(),
        device: "Chromecast".to_string(),
        device_id: "username-derived-device-id".to_string(),
        version: "0.18.0".to_string(),
        token: None,
    };

    service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &stored_auth,
            "jellyfin-token".to_string(),
            "original-jellyfin-user-id".to_string(),
            None,
        )
        .await
        .unwrap();

    let incoming_device = Device {
        client: "Jellyfin Android TV".to_string(),
        device: "Chromecast".to_string(),
        device_id: "userid-derived-device-id".to_string(),
        version: "0.18.0".to_string(),
    };

    let before = service
        .get_user_sessions(&user.id, Some(incoming_device.clone()))
        .await
        .unwrap();
    assert!(before.is_empty(), "precondition: strict lookup should miss");

    let rebound = service
        .rebind_android_tv_device_sessions_if_needed(&user.id, &incoming_device)
        .await
        .unwrap();
    assert!(rebound, "android tv device id should be rebound");

    let after = service
        .get_user_sessions(&user.id, Some(incoming_device.clone()))
        .await
        .unwrap();
    assert_eq!(after.len(), 1, "strict lookup should succeed after rebind");
    assert_eq!(after[0].0.device.device_id, incoming_device.device_id);
}

#[tokio::test]
async fn test_android_tv_rebind_collapses_multiple_stale_device_ids_for_same_user() {
    let (pool, service) = setup_service().await;
    insert_test_server(&pool, "Test Server", "http://localhost:8096").await;

    let user = service
        .get_or_create_user("androidtv-multi", &"testpass".into())
        .await
        .unwrap();

    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"mappedpass".into(),
            None,
        )
        .await
        .unwrap();

    for old_device_id in ["old-device-id-1", "old-device-id-2", "old-device-id-3"] {
        let stored_auth = Authorization {
            client: "Jellyfin Android TV".to_string(),
            device: "Chromecast".to_string(),
            device_id: old_device_id.to_string(),
            version: "0.18.0".to_string(),
            token: None,
        };

        service
            .store_authorization_session(
                &user.id,
                "http://localhost:8096",
                &stored_auth,
                format!("token-{old_device_id}"),
                "original-jellyfin-user-id".to_string(),
                None,
            )
            .await
            .unwrap();
    }

    let incoming_device = Device {
        client: "Jellyfin Android TV".to_string(),
        device: "Chromecast".to_string(),
        device_id: "incoming-device-id".to_string(),
        version: "0.18.0".to_string(),
    };

    let before = service
        .get_user_sessions(&user.id, Some(incoming_device.clone()))
        .await
        .unwrap();
    assert!(before.is_empty(), "precondition: strict lookup should miss");

    let rebound = service
        .rebind_android_tv_device_sessions_if_needed(&user.id, &incoming_device)
        .await
        .unwrap();
    assert!(rebound, "android tv stale sessions should be collapsed");

    let after = service
        .get_user_sessions(&user.id, Some(incoming_device.clone()))
        .await
        .unwrap();
    assert_eq!(
        after.len(),
        1,
        "collapse should leave one canonical session"
    );
    assert_eq!(after[0].0.device.device_id, incoming_device.device_id);

    let all_sessions = service.get_user_sessions(&user.id, None).await.unwrap();
    assert_eq!(all_sessions.len(), 1, "stale device ids should be pruned");
}

#[tokio::test]
async fn test_android_tv_rebind_scope_does_not_touch_other_users() {
    let (pool, service) = setup_service().await;
    insert_test_server(&pool, "Test Server", "http://localhost:8096").await;

    let user_a = service
        .get_or_create_user("androidtv-a", &"testpass".into())
        .await
        .unwrap();
    let user_b = service
        .get_or_create_user("androidtv-b", &"testpass".into())
        .await
        .unwrap();

    for user in [&user_a, &user_b] {
        service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser",
                &"mappedpass".into(),
                None,
            )
            .await
            .unwrap();
    }

    for old_device_id in ["old-device-id-1", "old-device-id-2"] {
        let stored_auth = Authorization {
            client: "Jellyfin Android TV".to_string(),
            device: "Chromecast".to_string(),
            device_id: old_device_id.to_string(),
            version: "0.18.0".to_string(),
            token: None,
        };

        service
            .store_authorization_session(
                &user_a.id,
                "http://localhost:8096",
                &stored_auth,
                format!("token-a-{old_device_id}"),
                "original-jellyfin-user-id-a".to_string(),
                None,
            )
            .await
            .unwrap();

        service
            .store_authorization_session(
                &user_b.id,
                "http://localhost:8096",
                &stored_auth,
                format!("token-b-{old_device_id}"),
                "original-jellyfin-user-id-b".to_string(),
                None,
            )
            .await
            .unwrap();
    }

    let incoming_device = Device {
        client: "Jellyfin Android TV".to_string(),
        device: "Chromecast".to_string(),
        device_id: "incoming-device-id".to_string(),
        version: "0.18.0".to_string(),
    };

    let rebound = service
        .rebind_android_tv_device_sessions_if_needed(&user_a.id, &incoming_device)
        .await
        .unwrap();
    assert!(rebound);

    let user_a_sessions = service.get_user_sessions(&user_a.id, None).await.unwrap();
    assert_eq!(user_a_sessions.len(), 1);
    assert_eq!(
        user_a_sessions[0].0.device.device_id,
        incoming_device.device_id
    );

    let user_b_sessions = service.get_user_sessions(&user_b.id, None).await.unwrap();
    assert_eq!(
        user_b_sessions.len(),
        2,
        "other users must remain untouched"
    );
    let user_b_device_ids = user_b_sessions
        .iter()
        .map(|(session, _)| session.device.device_id.as_str())
        .collect::<Vec<_>>();
    assert!(user_b_device_ids.contains(&"old-device-id-1"));
    assert!(user_b_device_ids.contains(&"old-device-id-2"));
}

#[tokio::test]
async fn test_android_tv_rebind_is_not_applied_to_other_clients() {
    let (pool, service) = setup_service().await;
    insert_test_server(&pool, "Test Server", "http://localhost:8096").await;

    let user = service
        .get_or_create_user("webuser", &"testpass".into())
        .await
        .unwrap();

    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"mappedpass".into(),
            None,
        )
        .await
        .unwrap();

    let stored_auth = Authorization {
        client: "Jellyfin Web".to_string(),
        device: "Firefox".to_string(),
        device_id: "web-old-device-id".to_string(),
        version: "10.10.7".to_string(),
        token: None,
    };

    service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &stored_auth,
            "jellyfin-token".to_string(),
            "original-jellyfin-user-id".to_string(),
            None,
        )
        .await
        .unwrap();

    let incoming_device = Device {
        client: "Jellyfin Web".to_string(),
        device: "Firefox".to_string(),
        device_id: "web-new-device-id".to_string(),
        version: "10.10.7".to_string(),
    };

    let rebound = service
        .rebind_android_tv_device_sessions_if_needed(&user.id, &incoming_device)
        .await
        .unwrap();
    assert!(!rebound, "non-android clients must not be rebound");

    let old_match = service
        .get_user_sessions(
            &user.id,
            Some(Device {
                client: "Jellyfin Web".to_string(),
                device: "Firefox".to_string(),
                device_id: "web-old-device-id".to_string(),
                version: "10.10.7".to_string(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(old_match.len(), 1);

    let new_match = service
        .get_user_sessions(&user.id, Some(incoming_device))
        .await
        .unwrap();
    assert_eq!(new_match.len(), 0);
}

#[tokio::test]
async fn test_get_user_sessions_by_virtual_token() {
    let (pool, service) = setup_service().await;
    insert_test_server(&pool, "Test Server", "http://localhost:8096").await;

    // Create user
    let user = service
        .get_or_create_user("testuser", &"testpass".into())
        .await
        .unwrap();

    // Add server mapping
    let _mapping_id = service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"mappedpass".into(),
            None,
        )
        .await
        .unwrap();

    // Create authorization
    let auth = Authorization {
        client: "Test Client".to_string(),
        device: "Test Device".to_string(),
        device_id: "test-device-id".to_string(),
        version: "1.0.0".to_string(),
        token: None,
    };

    let jellyfin_token = "test-jellyfin-token".to_string();

    // Store authorization session
    let _session_id = service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &auth,
            jellyfin_token.clone(),
            "original-jellyfin-user-id-2".to_string(),
            None,
        )
        .await
        .unwrap();

    // Test getting user sessions by virtual token
    let user_sessions = service
        .get_user_sessions_by_virtual_token(&user.virtual_key)
        .await
        .unwrap()
        .unwrap();

    let (retrieved_user, sessions) = user_sessions;
    assert_eq!(retrieved_user.original_username, "testuser");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].0.device.client, "Test Client");
    assert_eq!(sessions[0].0.server_url, "http://localhost:8096");
    assert_eq!(sessions[0].1.name, "Test Server");
    assert_eq!(
        sessions[0].1.url.as_str().trim_end_matches('/'),
        "http://localhost:8096"
    );
    assert_eq!(sessions[0].1.priority, 100);
}

#[tokio::test]
async fn test_multiple_servers_with_priority() {
    let (pool, service) = setup_service().await;

    // Create servers
    sqlx::query(
        r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
    )
    .bind("Server 1")
    .bind("http://localhost:8096")
    .bind(100)
    .bind(chrono::Utc::now())
    .bind(chrono::Utc::now())
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
    )
    .bind("Server 2")
    .bind("http://localhost:8097")
    .bind(200)
    .bind(chrono::Utc::now())
    .bind(chrono::Utc::now())
    .execute(&pool)
    .await
    .unwrap();

    // Create user
    let user = service
        .get_or_create_user("testuser", &"testpass".into())
        .await
        .unwrap();

    // Add server mappings
    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser1",
            &"mappedpass1".into(),
            None,
        )
        .await
        .unwrap();

    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8097",
            "mappeduser2",
            &"mappedpass2".into(),
            None,
        )
        .await
        .unwrap();

    // Create authorizations for both servers
    let auth1 = Authorization {
        client: "Test Client".to_string(),
        device: "Test Device".to_string(),
        device_id: "test-device-1".to_string(),
        version: "1.0.0".to_string(),
        token: None,
    };

    let auth2 = Authorization {
        client: "Test Client".to_string(),
        device: "Test Device".to_string(),
        device_id: "test-device-2".to_string(),
        version: "1.0.0".to_string(),
        token: None,
    };

    service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &auth1,
            "jellyfin-token-1".to_string(),
            "original-jellyfin-user-id-1".to_string(),
            None,
        )
        .await
        .unwrap();

    service
        .store_authorization_session(
            &user.id,
            "http://localhost:8097",
            &auth2,
            "jellyfin-token-2".to_string(),
            "original-jellyfin-user-id-2".to_string(),
            None,
        )
        .await
        .unwrap();

    // Test getting all authorization sessions for the user
    let user_sessions = service
        .get_user_sessions_by_virtual_token(&user.virtual_key)
        .await
        .unwrap()
        .unwrap();

    let (retrieved_user, sessions) = user_sessions;
    assert_eq!(retrieved_user.original_username, "testuser");
    assert_eq!(sessions.len(), 2);
    // Should be sorted by priority (descending), so Server 2 should come first
    assert_eq!(sessions[0].1.name, "Server 2");
    assert_eq!(sessions[0].1.priority, 200);
    assert_eq!(sessions[1].1.name, "Server 1");
    assert_eq!(sessions[1].1.priority, 100);
}

#[tokio::test]
async fn test_cascade_delete_sessions_on_mapping_delete() {
    let (pool, service) = setup_service().await;
    insert_test_server(&pool, "Server 1", "http://localhost:8096").await;

    // Create user
    let user = service
        .get_or_create_user("testuser", &"testpass".into())
        .await
        .unwrap();

    // Add mapping
    let mapping_id = service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"mappedpass".into(),
            None,
        )
        .await
        .unwrap();

    // Store session
    let auth = Authorization {
        client: "Test Client".to_string(),
        device: "Test Device".to_string(),
        device_id: "test-device-id".to_string(),
        version: "1.0.0".to_string(),
        token: None,
    };

    service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &auth,
            "jellyfin-token".to_string(),
            "original-jellyfin-user-id".to_string(),
            None,
        )
        .await
        .unwrap();

    // Pre-check session exists
    let sessions_before = service
        .get_user_sessions_by_virtual_token(&user.virtual_key)
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(sessions_before.len(), 1);

    // Delete mapping (authorization sessions are removed explicitly before FK cascade backup)
    let deleted = service.delete_server_mapping(mapping_id).await.unwrap();
    assert!(deleted);

    // Session should now be gone
    let sessions_after = service
        .get_user_sessions_by_virtual_token(&user.virtual_key)
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(
        sessions_after.len(),
        0,
        "Session should be deleted with the mapping"
    );
}

#[tokio::test]
async fn test_delete_all_sessions_for_user() {
    let (pool, service) = setup_service().await;

    // Insert two servers
    for (name, url) in [
        ("Server 1", "http://localhost:8096"),
        ("Server 2", "http://localhost:8097"),
    ] {
        insert_test_server(&pool, name, url).await;
    }

    // Create user
    let user = service
        .get_or_create_user("testuser", &"testpass".into())
        .await
        .unwrap();

    // Add mappings for both servers
    for url in ["http://localhost:8096", "http://localhost:8097"] {
        service
            .add_server_mapping(&user.id, url, "mappeduser", &"mappedpass".into(), None)
            .await
            .unwrap();
    }

    // Store two sessions
    for (i, url) in ["http://localhost:8096", "http://localhost:8097"]
        .iter()
        .enumerate()
    {
        let auth = Authorization {
            client: format!("Client {}", i + 1),
            device: "Test Device".to_string(),
            device_id: format!("device-{}", i + 1),
            version: "1.0.0".to_string(),
            token: None,
        };
        service
            .store_authorization_session(
                &user.id,
                url,
                &auth,
                format!("token-{}", i + 1),
                format!("orig-user-{}", i + 1),
                None,
            )
            .await
            .unwrap();
    }

    // Verify 2 sessions exist
    let sessions_before = service
        .get_user_sessions_by_virtual_token(&user.virtual_key)
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(sessions_before.len(), 2);

    // Delete all sessions
    let deleted_count = service
        .delete_all_sessions_for_user(&user.id)
        .await
        .unwrap();
    assert_eq!(deleted_count, 2);

    let sessions_after = service
        .get_user_sessions_by_virtual_token(&user.virtual_key)
        .await
        .unwrap()
        .unwrap()
        .1;
    assert!(sessions_after.is_empty());
}

#[tokio::test]
async fn test_add_server_mapping_upsert_preserves_mapping_id_and_sessions() {
    let (pool, service) = setup_service().await;
    insert_test_server(&pool, "Server 1", "http://localhost:8096").await;

    // Create user
    let user = service
        .get_or_create_user("testuser", &"testpass".into())
        .await
        .unwrap();

    // Initial mapping
    let mapping_id_1 = service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"mappedpass".into(),
            None,
        )
        .await
        .unwrap();

    // Store first session
    let auth1 = Authorization {
        client: "Jellyfin Web".to_string(),
        device: "Firefox".to_string(),
        device_id: "device-1".to_string(),
        version: "10.0.0".to_string(),
        token: None,
    };

    service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &auth1,
            "token-1".to_string(),
            "orig-user-1".to_string(),
            None,
        )
        .await
        .unwrap();

    // Re-add mapping for same user/server with identical credentials.
    // Should update in place and preserve sessions.
    let mapping_id_2 = service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"mappedpass".into(),
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        mapping_id_1, mapping_id_2,
        "Mapping id should remain stable across UPSERT"
    );

    // Ensure first session is still present after mapping update
    let sessions_after_update = service
        .get_user_sessions_by_virtual_token(&user.virtual_key)
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(
        sessions_after_update.len(),
        1,
        "Existing sessions should not be cascade-deleted on mapping update"
    );

    // Store second session for another device and ensure both exist
    let auth2 = Authorization {
        client: "Jellyfin Web".to_string(),
        device: "Firefox".to_string(),
        device_id: "device-2".to_string(),
        version: "10.0.0".to_string(),
        token: None,
    };

    service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &auth2,
            "token-2".to_string(),
            "orig-user-1".to_string(),
            None,
        )
        .await
        .unwrap();

    let final_sessions = service
        .get_user_sessions_by_virtual_token(&user.virtual_key)
        .await
        .unwrap()
        .unwrap()
        .1;

    assert_eq!(final_sessions.len(), 2);
}

#[tokio::test]
async fn test_add_server_mapping_username_change_deletes_sessions() {
    let (pool, service) = setup_service().await;
    insert_test_server(&pool, "Server 1", "http://localhost:8096").await;

    let user = service
        .get_or_create_user("testuser", &"testpass".into())
        .await
        .unwrap();

    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser-a",
            &"mappedpass-a".into(),
            None,
        )
        .await
        .unwrap();

    let auth = Authorization {
        client: "Jellyfin Web".to_string(),
        device: "Firefox".to_string(),
        device_id: "device-1".to_string(),
        version: "10.0.0".to_string(),
        token: None,
    };

    service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &auth,
            "token-1".to_string(),
            "orig-user-1".to_string(),
            None,
        )
        .await
        .unwrap();

    let sessions_before = service
        .get_user_sessions_by_virtual_token(&user.virtual_key)
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(sessions_before.len(), 1);

    // Change mapped username -> affected sessions are revoked by the service.
    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser-b",
            &"mappedpass-b".into(),
            None,
        )
        .await
        .unwrap();

    let sessions_after = service
        .get_user_sessions_by_virtual_token(&user.virtual_key)
        .await
        .unwrap()
        .unwrap()
        .1;

    assert_eq!(sessions_after.len(), 0);
}

#[tokio::test]
async fn test_add_server_mapping_password_change_deletes_sessions() {
    let (pool, service) = setup_service().await;
    insert_test_server(&pool, "Server 1", "http://localhost:8096").await;

    let user = service
        .get_or_create_user("testuser", &"testpass".into())
        .await
        .unwrap();

    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"mappedpass-a".into(),
            None,
        )
        .await
        .unwrap();

    let auth = Authorization {
        client: "Jellyfin Web".to_string(),
        device: "Firefox".to_string(),
        device_id: "device-1".to_string(),
        version: "10.0.0".to_string(),
        token: None,
    };

    service
        .store_authorization_session(
            &user.id,
            "http://localhost:8096",
            &auth,
            "token-1".to_string(),
            "orig-user-1".to_string(),
            None,
        )
        .await
        .unwrap();

    service
        .add_server_mapping(
            &user.id,
            "http://localhost:8096",
            "mappeduser",
            &"mappedpass-b".into(),
            None,
        )
        .await
        .unwrap();

    let sessions_after = service
        .get_user_sessions_by_virtual_token(&user.virtual_key)
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(sessions_after.len(), 0);
}
