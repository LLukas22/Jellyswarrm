//! Redeem an approved Quick Connect request. Remote authentication, local session
//! registration and response decoration are explicit stages, owned here rather
//! than by an incoming HTTP handler.
use crate::{
    application::accounts::AuthenticationService,
    auth::quick_connect::{QuickConnectSession, Redemption},
    protocol::authorization::Authorization,
    protocol::models::{AuthenticateResponse, SyncPlayUserAccessType},
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

/// `request` is the redeeming client's own authorization header, if parseable.
pub(crate) async fn redeem(
    state: &AuthenticationService,
    redemption: Redemption,
    request: Option<Authorization>,
) -> Result<AuthenticateResponse, QuickConnectError> {
    let authorization = redemption_authorization(request, redemption.session());
    redeem_with_hooks(
        state,
        redemption,
        authorization,
        RegistrationHooks::default(),
    )
    .await
}

/// The redeemed session's identity: the redeeming request's client metadata, or
/// the initiating request's when absent, and never a caller-supplied token.
fn redemption_authorization(
    request: Option<Authorization>,
    session: &QuickConnectSession,
) -> Authorization {
    let mut authorization = request.unwrap_or_else(|| {
        tracing::warn!(
            "Quick Connect authenticate request missing parseable authorization header, using initiate metadata"
        );
        Authorization {
            client: session.app_name.clone(),
            device: session.device_name.clone(),
            device_id: session.device_id.clone(),
            version: session.app_version.clone(),
            token: None,
        }
    });
    authorization.token = None;
    authorization
}

// The request owns only the reply receiver, never the worker/reservation
// lifetime. Cancellation lets the coordinator drain (not abort) every worker,
// including already-submitted SQL writes, before restoring the approval.
async fn redeem_with_hooks(
    state: &AuthenticationService,
    redemption: Redemption,
    authorization: Authorization,
    hooks: RegistrationHooks,
) -> Result<AuthenticateResponse, QuickConnectError> {
    let (reply, receiver) = tokio::sync::oneshot::channel();
    let state = state.clone();
    tokio::spawn(async move {
        let result = redeem_owned(&state, &redemption, authorization, &hooks).await;
        let success = result.is_ok();
        match reply.send(result) {
            Ok(()) if success => redemption.complete(),
            Ok(()) => drop(redemption),
            Err(Ok(response)) => {
                // The receiver disappeared during registration. Retire the undelivered
                // token before releasing the approval for a retry.
                if let Err(error) = state
                    .mapping_authenticator
                    .logout_login(&response.access_token)
                    .await
                {
                    tracing::error!(%error, "Could not retire undelivered Quick Connect login");
                }
                drop(redemption);
            }
            Err(Err(_)) => drop(redemption),
        }
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
    state: &AuthenticationService,
    redemption: &Redemption,
    authorization: Authorization,
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
    let upstream_auth = state
        .user_authorization
        .upstream_login_device(&authorization);
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
        let authorization = upstream_auth.clone();
        tasks.spawn(async move {
            let response = state
                .mapping_authenticator
                .authenticate_login(
                    &user,
                    &server,
                    &mapping,
                    None,
                    jellyfin_api::ClientInfo {
                        client: authorization.client,
                        device: authorization.device,
                        device_id: authorization.device_id,
                        version: authorization.version,
                    },
                )
                .await;
            (
                order,
                response.map(|login| crate::auth::mapping_authentication::BackendLogin {
                    server,
                    mapping: Some(mapping),
                    login,
                }),
            )
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
    successful.sort_by_key(|(order, _)| *order);
    if successful.is_empty() {
        return Err(QuickConnectError::Unauthorized);
    }
    hooks.before_registration().await;
    let successful: Vec<_> = successful.into_iter().map(|(_, backend)| backend).collect();
    let token = state
        .user_authorization
        .create_login(&user, &authorization, &upstream_auth.device_id, &successful)
        .await?;
    hooks.after_registration().await;
    let mut response = successful
        .into_iter()
        .next()
        .ok_or(QuickConnectError::Unauthorized)?
        .login
        .response;
    let server_id = state.settings.read().await.server_id.clone();
    response.server_id = server_id.clone();
    response.user.server_id = server_id.clone();
    response.session_info.server_id = server_id;
    response.user.id = user.id.to_string();
    response.user.name = user.original_username.clone();
    response.session_info.user_id = user.id.to_string();
    response.session_info.user_name = user.original_username.clone();
    response.user.policy.is_administrator = false;
    response.user.policy.sync_play_access = SyncPlayUserAccessType::CreateAndJoinGroups;
    response.access_token = token;
    crate::application::client_sessions::decorate_authentication(
        &state.client_sessions,
        &state.settings.read().await.server_id,
        &user,
        &authorization,
        &mut response,
    )
    .await;
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
        auth::{
            mapping_authentication::MappingAuthenticator,
            quick_connect::{QuickConnectStorage, RedemptionError},
        },
        config::{AppConfig, MediaStreamingMode},
        persistence::MIGRATOR,
        servers::ServerStorageService,
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
        let authorization =
            crate::auth::UserAuthorizationService::from_config(pool.clone(), &config)
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
        let authorization = Arc::new(authorization);
        let state = AuthenticationService {
            user_authorization: authorization.clone(),
            server_storage: Arc::new(servers.clone()),
            settings: crate::config::SettingsView::new(Arc::new(tokio::sync::RwLock::new(
                config.clone(),
            ))),
            client_sessions: Arc::new(crate::client_sessions::ClientSessionService::new(
                Default::default(),
            )),
            mapping_authenticator: MappingAuthenticator::new(
                authorization.clone(),
                reqwest::Client::new(),
                config.password,
            ),
        };
        let quick_connect = QuickConnectStorage::new();
        Mock::given(method("GET"))
            .and(path("/Users/Me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "Id":"remote-id", "Name":"Remote", "ServerId":"backend",
                "Policy":{"IsAdministrator":false,"SyncPlayAccess":"None"}
            })))
            .expect(2)
            .mount(&backend)
            .await;
        let view = quick_connect.initiate("device".into(), "TV".into(), "App".into(), "1".into());
        let secret = view.secret.as_str().into();
        assert!(quick_connect.approve(&view.code.as_str().into(), user.id.clone()));
        let reservation = quick_connect.begin_redemption(&secret).unwrap();
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
            quick_connect.begin_redemption(&secret),
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
            quick_connect.begin_redemption(&secret),
            Err(RedemptionError::NotApproved)
        ));
        after.resume.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), finished.notified())
            .await
            .unwrap();
        let retry = quick_connect.begin_redemption(&secret).unwrap();
        let response = redeem(&state, retry, Some(client)).await.unwrap();
        assert_ne!(response.access_token, user.virtual_key);
        assert!(matches!(
            quick_connect.begin_redemption(&secret),
            Err(RedemptionError::NotFound)
        ));
        let sessions = state
            .user_authorization
            .get_user_sessions(&user.id, None)
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1); // Only the delivered login survives the retry.
        assert_eq!(sessions[0].0.jellyfin_token, "saved-token");
        backend.verify().await;
    }

    #[test]
    fn quick_connect_preserves_the_redeeming_players_device_id() {
        let session = QuickConnectStorage::new().initiate(
            "session-device".to_string(),
            "Android TV".to_string(),
            "Jellyfin Android TV".to_string(),
            "1.0.0".to_string(),
        );

        let request = Authorization::parse(
            "MediaBrowser Client=\"Jellyfin Android TV\", Device=\"Chromecast\", DeviceId=\"androidtv-device-123\", Version=\"0.18.0\"",
        )
        .ok();

        let auth = redemption_authorization(request, &session);

        assert_eq!(auth.client, "Jellyfin Android TV");
        assert_eq!(auth.device, "Chromecast");
        assert_eq!(auth.version, "0.18.0");
        assert_eq!(auth.device_id, "androidtv-device-123");
        assert!(auth.token.is_none());
    }

    #[test]
    fn quick_connect_falls_back_to_session_metadata_without_headers() {
        let session = QuickConnectStorage::new().initiate(
            "session-device".to_string(),
            "Fallback Device".to_string(),
            "Unknown App".to_string(),
            "1.0.0".to_string(),
        );

        let auth = redemption_authorization(None, &session);

        assert_eq!(auth.device_id, "session-device");
        assert_eq!(auth.device, "Fallback Device");
        assert_eq!(auth.client, "Unknown App");
    }
}
