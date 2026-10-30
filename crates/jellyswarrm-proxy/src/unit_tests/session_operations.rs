//! Workflow tests compose real service capabilities without AppState or a router.
use super::*;
use crate::{
    config::AppConfig, libraries::VirtualLibraryService, media::MediaStorageService,
    persistence::MIGRATOR, servers::ServerStorageService, sessions::transport::ConnectionHub,
};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;

struct Fixture {
    pool: sqlx::SqlitePool,
    authorization: Arc<UserAuthorizationService>,
    media: Arc<MediaStorageService>,
    selection: super::super::url_selection::UrlSelection,
    sessions: Arc<ClientSessionService>,
    groups: Arc<SyncPlayService>,
    settings: SettingsView,
}

impl Fixture {
    async fn new() -> Self {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let media = Arc::new(MediaStorageService::new(pool.clone()));
        let servers = Arc::new(ServerStorageService::new(pool.clone()));
        let libraries = Arc::new(VirtualLibraryService::new(
            pool.clone(),
            (*servers).clone(),
            (*media).clone(),
        ));
        let hub = ConnectionHub::default();
        Self {
            authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            selection: super::super::url_selection::UrlSelection::new(
                media.clone(),
                servers,
                libraries,
            ),
            media,
            pool,
            sessions: Arc::new(ClientSessionService::new(hub.clone())),
            groups: Arc::new(SyncPlayService::with_transport(hub)),
            settings: SettingsView::new(Arc::new(tokio::sync::RwLock::new(AppConfig::default()))),
        }
    }

    fn operations(&self) -> SessionOperations {
        SessionOperations::new(
            self.authorization.clone(),
            self.sessions.clone(),
            self.groups.clone(),
            self.settings.clone(),
            access::MediaAccess::new(
                self.authorization.clone(),
                self.media.clone(),
                self.selection.clone(),
            ),
        )
    }

    async fn user(&self) -> User {
        self.authorization
            .create_user("Alice", &"".into())
            .await
            .unwrap()
    }

    async fn connect(&self, user: User, token: &str) -> (SessionContext, mpsc::Receiver<String>) {
        let id = self.sessions.ensure(&user, token, None).await;
        let context = SessionContext {
            user,
            session_id: id,
        };
        self.sessions
            .capabilities(&context.session_id, capabilities())
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        self.groups
            .register_websocket(context.session_id.clone(), tx)
            .await;
        rx.recv().await.unwrap(); // ForceKeepAlive is the connection handshake.
        (context, rx)
    }
}

fn capabilities() -> Capabilities {
    Capabilities {
        supports_media_control: true,
        supported_commands: vec!["SetVolume".into(), "SetRepeatMode".into()],
        ..Default::default()
    }
}

fn hint() -> Device {
    Device {
        client: "Browser".into(),
        device: "TV".into(),
        device_id: "cookie-device".into(),
        version: "1".into(),
    }
}

#[tokio::test]
async fn authentication_enforces_read_only_keys_and_never_trusts_a_hint_as_a_credential() {
    let f = Fixture::new().await;
    let user = f.user().await;
    let operations = f.operations();
    let headers = http::HeaderMap::new();
    let url = "http://proxy/socket".parse().unwrap();
    assert!(matches!(
        operations
            .authenticate(&headers, &url, false, Some(hint()))
            .await,
        Err(SessionOperationError::Unauthenticated)
    ));
    let url = format!("http://proxy/socket?api_key={}", user.virtual_key)
        .parse()
        .unwrap();
    let context = operations
        .authenticate(&headers, &url, false, Some(hint()))
        .await
        .unwrap();
    assert_eq!(
        f.sessions
            .get(&context.session_id)
            .await
            .unwrap()
            .device
            .device_id,
        "cookie-device"
    );
    let url = format!(
        "http://proxy/socket?api_key={}&deviceId=explicit",
        user.virtual_key
    )
    .parse()
    .unwrap();
    let context = operations
        .authenticate(&headers, &url, false, Some(hint()))
        .await
        .unwrap();
    assert_eq!(
        f.sessions
            .get(&context.session_id)
            .await
            .unwrap()
            .device
            .device_id,
        "explicit"
    );

    let key = f
        .authorization
        .create_api_key(&user.id, "reader")
        .await
        .unwrap();
    let url = format!("http://proxy/Sessions?api_key={}", key.access_token)
        .parse()
        .unwrap();
    assert!(operations
        .authenticate(&headers, &url, false, None)
        .await
        .is_ok());
    assert!(matches!(
        operations.authenticate(&headers, &url, true, None).await,
        Err(SessionOperationError::ReadOnly)
    ));
}

#[tokio::test]
async fn discovery_removes_invalid_credentials_and_their_group_membership() {
    let f = Fixture::new().await;
    let user = f.user().await;
    let (revoked, mut rx) = f.connect(user.clone(), "revoked-credential").await;
    assert!(f
        .groups
        .create_group(&revoked, "Group".into())
        .await
        .is_some());
    assert!(f.groups.is_group_member(&revoked.session_id).await);
    let snapshots = f.operations().snapshots(&user.id).await.unwrap();
    assert!(snapshots.is_empty());
    assert!(f.sessions.get(&revoked.session_id).await.is_none());
    assert!(!f.groups.is_group_member(&revoked.session_id).await);
    while rx.recv().await.is_some() {}
    assert!(!f.sessions.transport.is_connected(&revoked.session_id));
}

#[tokio::test]
async fn command_commit_rechecks_capabilities_and_group_membership_without_an_http_adapter() {
    let f = Fixture::new().await;
    let user = f.user().await;
    let (caller, mut rx) = f.connect(user.clone(), &user.virtual_key).await;
    let operations = f.operations();
    assert!(operations.target(&caller, &caller.session_id).await.is_ok());
    f.sessions
        .capabilities(&caller.session_id, Capabilities::default())
        .await
        .unwrap();
    assert!(matches!(
        operations
            .send(
                &caller,
                &caller.session_id,
                "Playstate",
                &json!({"Command":"Pause"})
            )
            .await,
        Err(SessionOperationError::NotControllable)
    ));
    assert!(rx.try_recv().is_err());
    f.sessions
        .capabilities(&caller.session_id, capabilities())
        .await
        .unwrap();
    f.groups
        .create_group(&caller, "Group".into())
        .await
        .unwrap();
    while rx.try_recv().is_ok() {}
    for (message_type, data) in [
        ("Playstate", json!({"Command":"Pause"})),
        ("GeneralCommand", json!({"Name":"SetRepeatMode"})),
    ] {
        assert!(matches!(
            operations
                .send(&caller, &caller.session_id, message_type, &data)
                .await,
            Err(SessionOperationError::Remote(
                RemoteCommandError::GroupPlayback
            ))
        ));
    }
    assert!(rx.try_recv().is_err());
    operations
        .send(
            &caller,
            &caller.session_id,
            "GeneralCommand",
            &json!({"Name":"SetVolume"}),
        )
        .await
        .unwrap();
    let message: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
    assert_eq!(message["Data"]["Name"], "SetVolume");
    operations.logout(&caller).await;
    assert!(!f.groups.is_group_member(&caller.session_id).await);
    assert!(f.sessions.get(&caller.session_id).await.is_none());
}

#[tokio::test]
async fn credential_lookup_failures_keep_storage_causes() {
    let f = Fixture::new().await;
    let user = f.user().await;
    let (caller, _rx) = f.connect(user.clone(), &user.virtual_key).await;
    f.pool.close().await;
    let error = match f.operations().target(&caller, &caller.session_id).await {
        Ok(_) => panic!("closed credential storage must fail"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SessionOperationError::Storage(sqlx::Error::PoolClosed)
    ));
    assert!(std::error::Error::source(&error)
        .unwrap()
        .is::<sqlx::Error>());
}
