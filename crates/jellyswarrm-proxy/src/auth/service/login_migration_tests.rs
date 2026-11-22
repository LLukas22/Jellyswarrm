//! Exercise upgrades with rows already present, including installations that
//! applied the first per-login migration before switching to this schema.
use super::*;
use crate::persistence::MIGRATOR;

#[tokio::test]
async fn upgrade_retires_only_password_sessions_and_preserves_named_keys() {
    for before in [20261011010000, 20261121000000] {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let previous = sqlx::migrate::Migrator {
            migrations: MIGRATOR
                .iter()
                .filter(|m| m.version <= before)
                .cloned()
                .collect(),
            ..sqlx::migrate::Migrator::DEFAULT
        };
        previous.run(&pool).await.unwrap();
        let mut service = UserAuthorizationService::new(pool.clone());
        let user = service
            .create_user("owner", &"secret".into())
            .await
            .unwrap();
        sqlx::query("INSERT INTO virtual_user_api_keys(user_id,app_name,access_token) VALUES(?,'Seerr','existing-key')").bind(&user.id).execute(&pool).await.unwrap();
        let encrypted = service
            .encrypt_session_token("encrypted-password-session".into())
            .unwrap();

        for (i, kind, format, token) in [
            (1, "password", "legacy", "plaintext-password-session"),
            (2, "password", "session_v1", encrypted.as_str()),
            (3, "quick_connect", "legacy", "durable-quick-connect"),
        ] {
            let url = format!("http://backend-{i}");
            sqlx::query("INSERT INTO servers(id,name,url) VALUES(?,?,?)")
                .bind(i)
                .bind(format!("Server {i}"))
                .bind(&url)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO server_mappings(id,user_id,server_id,server_url,mapped_username,mapped_password,auth_method,encrypted_token)
                VALUES(?,?,?,?,'remote','saved',?,?)")
                .bind(i).bind(&user.id).bind(i).bind(&url).bind(kind)
                .bind((kind=="quick_connect").then_some("mapping-credential")).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO authorization_sessions(user_id,mapping_id,server_url,client,device,device_id,version,jellyfin_token,original_user_id,token_format)
                VALUES(?,?,?,'test','tv','id','1',?,'remote',?)")
                .bind(&user.id).bind(i).bind(&url).bind(token).bind(format).execute(&pool).await.unwrap();
        }
        if before == 20261121000000 {
            sqlx::query("INSERT INTO client_logins(user_id,token_hash) VALUES(?,zeroblob(32))")
                .bind(&user.id)
                .execute(&pool)
                .await
                .unwrap();
        }

        MIGRATOR.run(&pool).await.unwrap();
        for _ in 0..2 {
            service.enable_session_encryption(&[7; 64]).await.unwrap();
        }
        let retired: Vec<String> = sqlx::query_scalar(
            "SELECT encrypted_token FROM pending_upstream_logouts ORDER BY server_url",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(retired.len(), 2);
        assert_ne!(retired[0], "plaintext-password-session");
        assert_eq!(
            service.decrypt_session_token(retired[0].clone()).unwrap(),
            "plaintext-password-session"
        );
        assert_eq!(
            service.decrypt_session_token(retired[1].clone()).unwrap(),
            "encrypted-password-session"
        );
        let shared: String = sqlx::query_scalar("SELECT virtual_key FROM users WHERE id=?")
            .bind(&user.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(service.resolve_credential(&shared).await.unwrap().is_none());
        let (_, CallerCredential::ApplicationKey(login)) = service
            .resolve_credential("existing-key")
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("missing key login")
        };
        assert!(service
            .sessions_for_credential(&user.id, CallerCredential::ApplicationKey(login))
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM client_logins")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_mappings")
                .fetch_one(&pool)
                .await
                .unwrap(),
            3
        );
    }
}

#[tokio::test]
async fn every_cascade_retires_each_owned_token_once() {
    for delete in [
        "DELETE FROM client_logins",
        "DELETE FROM server_mappings",
        "DELETE FROM virtual_user_api_keys",
        "DELETE FROM users",
        "DELETE FROM servers",
    ] {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());
        let user = service.fixture_user("owner", &"".into()).await.unwrap();
        let (key, _) = service.create_api_key(&user, "reader").await.unwrap();
        let (_, CallerCredential::ApplicationKey(id)) = service
            .resolve_credential(&key.access_token)
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("missing key login")
        };
        for (i, owned) in [(1, true), (2, false)] {
            let url = format!("http://server{i}");
            sqlx::query("INSERT INTO servers(id,name,url) VALUES(?,?,?)")
                .bind(i)
                .bind(&url)
                .bind(&url)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO server_mappings(id,user_id,server_id,server_url,mapped_username,mapped_password) VALUES(?,?,?,?,'remote','saved')")
                .bind(i).bind(&user.id).bind(i).bind(&url).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO login_sessions(user_id,login_id,mapping_id,server_url,upstream_device_id,jellyfin_token,owned,original_user_id)
                VALUES(?,?,?,?,'device',?,?,'remote')")
                .bind(&user.id).bind(id).bind(i).bind(&url).bind(service.encrypt_session_token(format!("token{i}")).unwrap()).bind(owned).execute(&pool).await.unwrap();
        }
        sqlx::query(delete).execute(&pool).await.unwrap();
        let retired: Vec<String> =
            sqlx::query_scalar("SELECT encrypted_token FROM pending_upstream_logouts")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(retired.len(), 1, "{delete}");
        assert_eq!(
            service.decrypt_session_token(retired[0].clone()).unwrap(),
            "token1"
        );
    }
}

#[tokio::test]
async fn idle_expiry_spares_active_players_and_keys_and_rejects_a_revoked_issuer() {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let service = UserAuthorizationService::new(pool.clone());
    let idle = service.fixture_user("idle", &"".into()).await.unwrap();
    let active = service.fixture_user("active", &"".into()).await.unwrap();
    let (key, _) = service.create_api_key(&idle, "integration").await.unwrap();
    sqlx::query("UPDATE client_logins SET last_used_at=unixepoch()-172800")
        .execute(&pool)
        .await
        .unwrap();
    service.expire_idle_logins(0).await.unwrap();
    assert!(service.login_exists(&idle.id, idle.login_id).await.unwrap());
    assert!(service
        .resolve_credential(&active.fixture_token)
        .await
        .unwrap()
        .is_some());
    service.expire_idle_logins(1).await.unwrap();
    assert!(service
        .resolve_credential(&active.fixture_token)
        .await
        .unwrap()
        .is_some());
    assert!(service
        .resolve_credential(&key.access_token)
        .await
        .unwrap()
        .is_some());
    assert!(service
        .resolve_credential(&idle.fixture_token)
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        service.create_api_key(&idle, "delayed request").await,
        Err(sqlx::Error::RowNotFound)
    ));
}
