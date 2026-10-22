//! Redeem an approved Quick Connect request. Remote authentication, local session
//! registration and response decoration are explicit stages, owned here rather
//! than by an incoming HTTP handler.
use crate::{
    auth::quick_connect::Redemption,
    models::{AuthenticateResponse, Authorization, SyncPlayUserAccessType},
    AppState,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum QuickConnectError {
    #[error("Quick Connect user or mappings are unavailable")]
    Unauthorized,
    #[error("No backend servers are configured")]
    NoServers,
    #[error("Quick Connect storage failure")]
    Storage(#[from] sqlx::Error),
    #[error("Quick Connect coordinator stopped")]
    CoordinatorStopped(#[source] tokio::sync::oneshot::error::RecvError),
}

pub(crate) async fn redeem(
    state: &AppState,
    redemption: Redemption,
    authorization: Authorization,
) -> Result<AuthenticateResponse, QuickConnectError> {
    redeem_with_hooks(
        state,
        redemption,
        authorization,
        RegistrationHooks::default(),
    )
    .await
}

// The request owns only the reply receiver, never the worker/reservation
// lifetime. Cancellation lets the coordinator drain (not abort) every worker,
// including already-submitted SQL writes, before restoring the approval.
async fn redeem_with_hooks(
    state: &AppState,
    redemption: Redemption,
    authorization: Authorization,
    hooks: RegistrationHooks,
) -> Result<AuthenticateResponse, QuickConnectError> {
    let (reply, receiver) = tokio::sync::oneshot::channel();
    let state = state.clone();
    tokio::spawn(async move {
        let result = redeem_owned(&state, redemption, authorization, &reply, &hooks).await;
        let _ = reply.send(result);
        #[cfg(test)]
        if let Some(finished) = hooks.finished {
            finished.notify_one();
        }
    });
    receiver
        .await
        .map_err(QuickConnectError::CoordinatorStopped)?
}

async fn redeem_owned(
    state: &AppState,
    redemption: Redemption,
    authorization: Authorization,
    reply: &tokio::sync::oneshot::Sender<Result<AuthenticateResponse, QuickConnectError>>,
    hooks: &RegistrationHooks,
) -> Result<AuthenticateResponse, QuickConnectError> {
    let user = state
        .user_authorization
        .get_user_by_id(redemption.user_id())
        .await?
        .ok_or(QuickConnectError::Unauthorized)?;
    let mut servers = state.server_storage.list_servers().await?;
    if servers.is_empty() {
        return Err(QuickConnectError::NoServers);
    }
    let mappings = state
        .user_authorization
        .list_server_mappings(&user.id)
        .await?;
    if mappings.is_empty() {
        return Err(QuickConnectError::Unauthorized);
    }
    // Only this coordinator owns these workers. It is not aborted when the
    // request is cancelled, and drains the set before dropping the reservation.
    let mut tasks = tokio::task::JoinSet::new();
    for (order, mapping) in mappings.into_iter().enumerate() {
        let Some(position) = servers
            .iter()
            .position(|server| server.id == mapping.server_id)
        else {
            continue;
        };
        let server = servers.remove(position);
        let state = state.clone();
        let user = user.clone();
        let authorization = authorization.clone();
        let hooks = hooks.clone();
        tasks.spawn(async move {
            let result =
                authenticate_and_register(&state, &user, &server, &mapping, &authorization, &hooks)
                    .await;
            (order, result)
        });
    }
    let mut successful = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok((order, Ok(response))) => successful.push((order, response)),
            Ok((_, Err(error))) => {
                tracing::debug!(%error, "Quick Connect backend authentication failed")
            }
            Err(error) => tracing::warn!(%error, "Quick Connect authentication task failed"),
        }
    }
    if !reply.is_closed() {
        redemption.complete();
    }
    // Otherwise Drop restores approval only now, after every worker terminated.
    // Preserve mapping priority/order, not completion timing.
    successful.sort_by_key(|(order, _)| *order);
    successful
        .into_iter()
        .next()
        .map(|(_, response)| response)
        .ok_or(QuickConnectError::Unauthorized)
}

async fn authenticate_and_register(
    state: &AppState,
    user: &crate::auth::service::User,
    server: &crate::servers::Server,
    mapping: &crate::auth::service::ServerMapping,
    authorization: &Authorization,
    hooks: &RegistrationHooks,
) -> Result<AuthenticateResponse, crate::auth::mapping_authentication::MappingAuthenticationError> {
    let mut response = state
        .mapping_authenticator()
        .await
        .authenticate_for_client(
            user,
            server,
            mapping,
            jellyfin_api::ClientInfo {
                client: authorization.client.clone(),
                device: authorization.device.clone(),
                device_id: authorization.device_id.clone(),
                version: authorization.version.clone(),
            },
        )
        .await?;
    let mut backend_authorization = authorization.clone();
    backend_authorization.token = Some(response.access_token.clone());
    hooks.before_registration().await;
    state
        .user_authorization
        .store_authorization_session(
            &user.id,
            server,
            &backend_authorization,
            response.access_token.clone(),
            response.user.id.clone(),
            None,
        )
        .await?;
    hooks.after_registration().await;

    let server_id = state.settings.read().await.server_id.clone();
    response.server_id = server_id.clone();
    response.user.server_id = server_id.clone();
    response.session_info.server_id = server_id;
    response.user.id = user.id.clone();
    response.user.name = user.original_username.clone();
    response.session_info.user_id = user.id.clone();
    response.session_info.user_name = user.original_username.clone();
    response.user.policy.is_administrator = false;
    response.user.policy.sync_play_access = SyncPlayUserAccessType::CreateAndJoinGroups;
    response.access_token = user.virtual_key.clone();
    crate::sessions::decorate_authentication(state, user, authorization, &mut response).await;
    Ok(response)
}

/// Test-only scheduling points around the real registration call. Production
/// carries no gates and executes the same coordinator/worker code.
#[derive(Clone, Default)]
struct RegistrationHooks {
    #[cfg(test)]
    before: Option<std::sync::Arc<RegistrationPause>>,
    #[cfg(test)]
    after: Option<std::sync::Arc<RegistrationPause>>,
    #[cfg(test)]
    finished: Option<std::sync::Arc<tokio::sync::Notify>>,
}
impl RegistrationHooks {
    async fn before_registration(&self) {
        #[cfg(test)]
        if let Some(pause) = &self.before {
            pause.wait().await;
        }
    }
    async fn after_registration(&self) {
        #[cfg(test)]
        if let Some(pause) = &self.after {
            pause.wait().await;
        }
    }
}
#[cfg(test)]
#[derive(Default)]
struct RegistrationPause {
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}
#[cfg(test)]
impl RegistrationPause {
    async fn wait(&self) {
        self.entered.notify_one();
        self.resume.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::playback::sessions::SessionStorage,
        auth::quick_connect::{QuickConnectStorage, RedemptionError},
        config::{AppConfig, MediaStreamingMode, MIGRATOR},
        libraries::VirtualLibraryService,
        media::MediaStorageService,
        processors::ProxyProcessors,
        servers::ServerStorageService,
        DataContext,
    };
    use std::sync::Arc;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    #[tokio::test]
    async fn cancelled_caller_cannot_retry_until_registration_workers_finish() {
        let backend = MockServer::start().await;
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let config = AppConfig::default();
        let authorization = crate::app::authorization::build_authorization(pool.clone(), &config)
            .await
            .unwrap();
        let user = authorization
            .create_user("local", &"password".into())
            .await
            .unwrap();
        let servers = ServerStorageService::new(pool.clone());
        let id = servers
            .add_server("Backend", &backend.uri(), 100, MediaStreamingMode::Proxy)
            .await
            .unwrap();
        let server = servers.get_server_by_id(id).await.unwrap().unwrap();
        authorization
            .add_quick_connect_mapping(
                &user.id,
                &server,
                "Remote",
                "remote-id",
                "saved-token",
                &user.local_credential.mapping_key(),
            )
            .await
            .unwrap();
        let media = MediaStorageService::new(pool.clone());
        let data = DataContext {
            user_authorization: Arc::new(authorization),
            server_storage: Arc::new(servers.clone()),
            media_storage: Arc::new(media.clone()),
            virtual_library_service: Arc::new(VirtualLibraryService::new(
                pool.clone(),
                servers,
                media,
            )),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(config)),
        };
        let state = AppState::new(
            reqwest::Client::new(),
            reqwest::Client::new(),
            data.clone(),
            ProxyProcessors::new(data),
            QuickConnectStorage::new(),
        );
        Mock::given(method("GET"))
            .and(path("/Users/Me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "Id":"remote-id", "Name":"Remote", "ServerId":"backend",
                "Policy":{"IsAdministrator":false,"SyncPlayAccess":"None"}
            })))
            .expect(2)
            .mount(&backend)
            .await;
        let view =
            state
                .quick_connect
                .initiate("device".into(), "TV".into(), "App".into(), "1".into());
        let secret = view.secret.as_str().into();
        assert!(state
            .quick_connect
            .approve(&view.code.as_str().into(), user.id.clone()));
        let reservation = state.quick_connect.begin_redemption(&secret).unwrap();
        let before = Arc::new(RegistrationPause::default());
        let after = Arc::new(RegistrationPause::default());
        let finished = Arc::new(tokio::sync::Notify::new());
        let hooks = RegistrationHooks {
            before: Some(before.clone()),
            after: Some(after.clone()),
            finished: Some(finished.clone()),
        };
        let client = Authorization {
            client: "App".into(),
            device: "TV".into(),
            device_id: "device".into(),
            version: "1".into(),
            token: None,
        };
        let caller = {
            let state = state.clone();
            let client = client.clone();
            tokio::spawn(async move { redeem_with_hooks(&state, reservation, client, hooks).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), before.entered.notified())
            .await
            .unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM authorization_sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
        assert!(matches!(
            state.quick_connect.begin_redemption(&secret),
            Err(RedemptionError::NotApproved)
        ));
        before.resume.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), after.entered.notified())
            .await
            .unwrap();
        // Real registration has committed, but its worker has not returned.
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM authorization_sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
        assert!(matches!(
            state.quick_connect.begin_redemption(&secret),
            Err(RedemptionError::NotApproved)
        ));
        after.resume.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), finished.notified())
            .await
            .unwrap();
        let retry = state.quick_connect.begin_redemption(&secret).unwrap();
        let response = redeem(&state, retry, client).await.unwrap();
        assert_eq!(response.access_token, user.virtual_key);
        assert!(matches!(
            state.quick_connect.begin_redemption(&secret),
            Err(RedemptionError::NotFound)
        ));
        let sessions = state
            .user_authorization
            .get_user_sessions(&user.id, None)
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1); // completed registration, then sequential upsert
        assert_eq!(sessions[0].0.jellyfin_token, "saved-token");
        backend.verify().await;
    }
}
