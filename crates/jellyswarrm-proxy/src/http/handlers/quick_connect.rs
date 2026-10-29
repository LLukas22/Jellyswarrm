//! Handle Quick Connect initiation, polling, authorization, and temporary request storage.
use crate::auth::user_id::UserId;
use crate::{models::Authorization, AppState};
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use hyper::StatusCode;
use jellyswarrm_macros::multi_case_struct;
use serde::Deserialize;
use sha1::{Digest, Sha1};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::auth::quick_connect::QuickConnectSession;
#[cfg(test)]
pub(crate) use crate::auth::quick_connect::QuickConnectStorage;

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct AuthorizeQuery {
    pub(crate) code: String,
    pub(crate) user_id: Option<UserId>,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct ConnectQuery {
    pub(crate) secret: String,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct QuickConnectAuthenticateRequest {
    pub(crate) secret: String,
}

fn parse_authorization_from_headers(headers: &HeaderMap) -> Option<Authorization> {
    if let Some(header) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        match Authorization::parse_with_legacy(header, true) {
            Ok(auth) => return Some(auth),
            Err(e) => warn!("Failed to parse Authorization header: {}", e),
        }
    }

    if let Some(header) = headers
        .get("x-emby-authorization")
        .and_then(|value| value.to_str().ok())
    {
        match Authorization::parse_with_legacy(header, true) {
            Ok(auth) => return Some(auth),
            Err(e) => warn!("Failed to parse X-Emby-Authorization header: {}", e),
        }
    }

    None
}

fn parse_client_info(headers: &HeaderMap) -> (String, String, String, String) {
    if let Some(auth) = parse_authorization_from_headers(headers) {
        return (auth.device_id, auth.device, auth.client, auth.version);
    }

    (
        Uuid::new_v4().to_string(),
        "Unknown Device".to_string(),
        "Unknown App".to_string(),
        "1.0.0".to_string(),
    )
}

fn is_android_tv_client(client: &str) -> bool {
    client.to_ascii_lowercase().contains("android tv")
}

fn normalize_android_tv_user_id(user_id: &str) -> String {
    Uuid::parse_str(user_id)
        .map(|id| id.hyphenated().to_string())
        .unwrap_or_else(|_| user_id.to_string())
}

fn derive_android_tv_user_device_id(base_device_id: &str, user_id: &str) -> String {
    let normalized_user_id = normalize_android_tv_user_id(user_id);
    let mut hasher = Sha1::new();
    hasher.update(base_device_id.as_bytes());
    hasher.update(b"+");
    hasher.update(normalized_user_id.as_bytes());
    hex::encode(hasher.finalize())
}

fn effective_quick_connect_authorization(
    headers: &HeaderMap,
    session: &QuickConnectSession,
    user_id: &str,
) -> Authorization {
    let request_authorization = parse_authorization_from_headers(headers);

    let mut authorization = request_authorization.unwrap_or_else(|| {
        warn!(
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

    if is_android_tv_client(&authorization.client) && !authorization.device_id.is_empty() {
        let original_device_id = authorization.device_id.clone();
        authorization.device_id =
            derive_android_tv_user_device_id(&authorization.device_id, user_id);
        debug!(
            "Derived Android TV user-scoped device id for Quick Connect: {} -> {}",
            original_device_id, authorization.device_id
        );
    }

    authorization
}

pub(crate) async fn handle_quick_connect_enabled() -> Result<Json<bool>, StatusCode> {
    Ok(Json(true))
}

pub(crate) async fn handle_quick_connect_initiate(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<QuickConnectSession>, StatusCode> {
    let (device_id, device_name, app_name, app_version) = parse_client_info(&headers);

    info!(
        "Initiating Quick Connect session for {} / {} ({})",
        app_name, device_name, device_id
    );

    let session = state
        .quick_connect
        .initiate(device_id, device_name, app_name, app_version);

    Ok(Json(session))
}

pub(crate) async fn handle_quick_connect_authorize(
    Query(params): Query<AuthorizeQuery>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<bool>, StatusCode> {
    let user_id = resolve_authorize_user_id(&state, &headers, params.user_id).await?;

    let success = state
        .quick_connect
        .approve(&params.code.as_str().into(), user_id.clone());

    if success {
        info!(
            "Authorized Quick Connect code {} for user {}",
            params.code, user_id
        );
        Ok(Json(true))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn resolve_authorize_user_id(
    state: &AppState,
    headers: &HeaderMap,
    user_id: Option<UserId>,
) -> Result<UserId, StatusCode> {
    let token = extract_virtual_token(headers).ok_or_else(|| {
        warn!("Quick Connect authorize called without a virtual token");
        StatusCode::BAD_REQUEST
    })?;

    let user = state
        .user_authorization
        .get_user_by_virtual_key(&token)
        .await
        .map_err(|e| {
            warn!("Failed to resolve user from virtual token: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // A userId parameter selects no authority of its own. Approval must always
    // belong to the authenticated caller, never an arbitrary local account.
    if user_id
        .as_ref()
        .is_some_and(|requested| requested != &user.id)
    {
        return Err(StatusCode::FORBIDDEN);
    }

    Ok(user.id)
}

fn extract_virtual_token(headers: &HeaderMap) -> Option<String> {
    if let Some(header) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(auth) = Authorization::parse(header) {
            if let Some(token) = auth.token {
                return Some(token);
            }
        }
    }

    if let Some(header) = headers
        .get("x-emby-authorization")
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(auth) = Authorization::parse_with_legacy(header, true) {
            if let Some(token) = auth.token {
                return Some(token);
            }
        }
    }

    if let Some(token) = headers
        .get("x-emby-token")
        .and_then(|value| value.to_str().ok())
    {
        return Some(token.to_string());
    }

    if let Some(token) = headers
        .get("x-mediabrowser-token")
        .and_then(|value| value.to_str().ok())
    {
        return Some(token.to_string());
    }

    None
}

pub(crate) async fn handle_quick_connect_connect(
    Query(params): Query<ConnectQuery>,
    State(state): State<AppState>,
) -> Result<Json<QuickConnectSession>, StatusCode> {
    if let Some(session) = state.quick_connect.poll(&params.secret.as_str().into()) {
        Ok(Json(session))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

pub(crate) async fn handle_authenticate_with_quick_connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<QuickConnectAuthenticateRequest>,
) -> Result<crate::sessions::AuthenticationResponse, StatusCode> {
    let redemption = state
        .quick_connect
        .begin_redemption(&request.secret.as_str().into())
        .map_err(|error| match error {
            crate::auth::quick_connect::RedemptionError::NotFound => StatusCode::NOT_FOUND,
            crate::auth::quick_connect::RedemptionError::NotApproved => StatusCode::UNAUTHORIZED,
        })?;
    let authorization =
        effective_quick_connect_authorization(&headers, redemption.session(), redemption.user_id());
    crate::application::quick_connect::redeem(
        &state.authentication_service().await,
        redemption,
        authorization,
    )
    .await
    .map(crate::sessions::authentication_response)
    .map_err(|error| match error {
        crate::application::quick_connect::QuickConnectError::Unauthorized => {
            StatusCode::UNAUTHORIZED
        }
        crate::application::quick_connect::QuickConnectError::NoServers => StatusCode::NOT_FOUND,
        crate::application::quick_connect::QuickConnectError::Storage(_)
        | crate::application::quick_connect::QuickConnectError::CoordinatorStopped(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_query_normalizes_user_uuid_on_deserialization() {
        for key in ["userId", "UserId"] {
            let uri = format!(
                "/QuickConnect/Authorize?code=123456&{key}=550E8400-E29B-41D4-A716-446655440000"
            )
            .parse()
            .unwrap();
            let Query(query) = Query::<AuthorizeQuery>::try_from_uri(&uri).unwrap();
            assert_eq!(
                query.user_id.unwrap(),
                UserId::new("550e8400e29b41d4a716446655440000")
            );
        }
    }
    use crate::{
        auth::service::{Device, UserAuthorizationService},
        config::{AppConfig, MediaStreamingMode},
        media::storage::MediaStorageService,
        models::{AuthenticateResponse, SessionInfo, SyncPlayUserAccessType, User, UserPolicy},
        persistence::MIGRATOR,
        playback::sessions::SessionStorage,
        processors::ProxyProcessors,
        servers::storage::ServerStorageService,
        AppState, DataContext,
    };
    use axum::{extract::Query, Json};
    use hyper::http::HeaderValue;
    use sqlx::SqlitePool;
    use std::{collections::HashMap, sync::Arc};
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    async fn create_test_app_state() -> AppState {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());

        let data_context = DataContext {
            paths: crate::config::Paths::temporary(),
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(server_storage.clone()),
            media_storage: Arc::new(media_storage.clone()),
            virtual_library_service: Arc::new(
                crate::libraries::service::VirtualLibraryService::new(
                    pool,
                    server_storage,
                    media_storage,
                ),
            ),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        };

        let processors = ProxyProcessors::new(data_context.clone());

        AppState::new(
            reqwest::Client::new(),
            reqwest::Client::new(),
            data_context,
            processors,
            QuickConnectStorage::new(),
        )
    }

    #[tokio::test]
    async fn authorize_cannot_use_user_id_as_a_substitute_for_caller_authentication() {
        let state = create_test_app_state().await;
        assert_eq!(
            resolve_authorize_user_id(&state, &HeaderMap::new(), Some("victim".into()))
                .await
                .unwrap_err(),
            StatusCode::BAD_REQUEST
        );
        let user = state
            .user_authorization
            .create_user("caller", &"password".into())
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-emby-token", user.virtual_key.parse().unwrap());
        assert_eq!(
            resolve_authorize_user_id(&state, &headers, Some("victim".into()))
                .await
                .unwrap_err(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            resolve_authorize_user_id(&state, &headers, Some(user.id.clone()))
                .await
                .unwrap(),
            user.id
        );
    }

    #[test]
    fn quick_connect_session_serializes_to_jellyfin_shape() {
        let session = QuickConnectStorage::new().initiate(
            "device-1".to_string(),
            "Test Device".to_string(),
            "Test App".to_string(),
            "1.2.3".to_string(),
        );

        let value = serde_json::to_value(session).unwrap();
        let obj = value.as_object().unwrap();

        assert!(obj.contains_key("Authenticated"));
        assert!(obj.contains_key("Secret"));
        assert!(obj.contains_key("Code"));
        assert!(obj.contains_key("DeviceId"));
        assert!(obj.contains_key("DeviceName"));
        assert!(obj.contains_key("AppName"));
        assert!(obj.contains_key("AppVersion"));
        assert!(obj.contains_key("DateAdded"));

        assert!(!obj.contains_key("UserId"));
        assert!(!obj.contains_key("ExpiresAt"));
    }

    #[test]
    fn quick_connect_dto_accepts_pascal_and_camel_secret() {
        let pascal: QuickConnectAuthenticateRequest =
            serde_json::from_str(r#"{"Secret":"abc"}"#).unwrap();
        let camel: QuickConnectAuthenticateRequest =
            serde_json::from_str(r#"{"secret":"xyz"}"#).unwrap();

        assert_eq!(pascal.secret, "abc");
        assert_eq!(camel.secret, "xyz");
    }

    #[test]
    fn authorize_query_accepts_pascal_and_camel_user_id() {
        let camel: AuthorizeQuery =
            serde_json::from_str(r#"{"code":"123456","userId":"user-1"}"#).unwrap();
        let pascal: AuthorizeQuery =
            serde_json::from_str(r#"{"Code":"123456","UserId":"user-2"}"#).unwrap();
        let missing: AuthorizeQuery = serde_json::from_str(r#"{"code":"123456"}"#).unwrap();

        assert_eq!(camel.code, "123456");
        assert_eq!(camel.user_id.as_deref(), Some("user-1"));
        assert_eq!(pascal.user_id.as_deref(), Some("user-2"));
        assert!(missing.user_id.is_none());
    }

    #[test]
    fn extract_virtual_token_supports_auth_and_token_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static(
                "MediaBrowser Client=\"Jellyfin Web\", Device=\"Firefox\", DeviceId=\"abc\", Version=\"10.10.7\", Token=\"token-1\"",
            ),
        );
        assert_eq!(extract_virtual_token(&headers).as_deref(), Some("token-1"));

        let mut x_emby_token = HeaderMap::new();
        x_emby_token.insert("x-emby-token", HeaderValue::from_static("token-2"));
        assert_eq!(
            extract_virtual_token(&x_emby_token).as_deref(),
            Some("token-2")
        );

        let mut media_browser_token = HeaderMap::new();
        media_browser_token.insert("x-mediabrowser-token", HeaderValue::from_static("token-3"));
        assert_eq!(
            extract_virtual_token(&media_browser_token).as_deref(),
            Some("token-3")
        );
    }

    #[test]
    fn detects_android_tv_clients() {
        assert!(is_android_tv_client("Jellyfin Android TV"));
        assert!(is_android_tv_client("Jellyfin Android TV (debug)"));
        assert!(!is_android_tv_client("Jellyfin Android"));
        assert!(!is_android_tv_client("Jellyfin Web"));
    }

    #[test]
    fn derives_android_tv_device_id_from_uuid_user_id() {
        let base_device_id = "androidtv-device-123";
        let simple_user_id = "550e8400e29b41d4a716446655440000";
        let derived = derive_android_tv_user_device_id(base_device_id, simple_user_id);

        assert_eq!(derived, "7094964e96c0e66e00671463e263fbdffce6b001");
    }

    #[test]
    fn quick_connect_uses_android_tv_user_scoped_device_id() {
        let session = QuickConnectStorage::new().initiate(
            "session-device".to_string(),
            "Android TV".to_string(),
            "Jellyfin Android TV".to_string(),
            "1.0.0".to_string(),
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static(
                "MediaBrowser Client=\"Jellyfin Android TV\", Device=\"Chromecast\", DeviceId=\"androidtv-device-123\", Version=\"0.18.0\"",
            ),
        );

        let auth = effective_quick_connect_authorization(
            &headers,
            &session,
            "550e8400e29b41d4a716446655440000",
        );

        assert_eq!(auth.client, "Jellyfin Android TV");
        assert_eq!(auth.device, "Chromecast");
        assert_eq!(auth.version, "0.18.0");
        assert_eq!(auth.device_id, "7094964e96c0e66e00671463e263fbdffce6b001");
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

        let headers = HeaderMap::new();
        let auth = effective_quick_connect_authorization(&headers, &session, "user-id");

        assert_eq!(auth.device_id, "session-device");
        assert_eq!(auth.device, "Fallback Device");
        assert_eq!(auth.client, "Unknown App");
    }

    #[tokio::test]
    async fn quick_connect_authentication_preserves_existing_web_session() {
        let state = create_test_app_state().await;
        let upstream = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/Users/AuthenticateByName"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(AuthenticateResponse {
                    user: User {
                        name: "mappeduser".to_string(),
                        server_id: "upstream-server".to_string(),
                        id: "upstream-user-id".to_string(),
                        policy: UserPolicy {
                            is_administrator: false,
                            sync_play_access: SyncPlayUserAccessType::None,
                            extra: HashMap::new(),
                        },
                        extra: HashMap::new(),
                    },
                    session_info: SessionInfo {
                        user_id: "upstream-user-id".to_string(),
                        user_name: "mappeduser".to_string(),
                        server_id: "upstream-server".to_string(),
                        extra: HashMap::new(),
                    },
                    access_token: "tv-upstream-token".to_string(),
                    server_id: "upstream-server".to_string(),
                }),
            )
            .mount(&upstream)
            .await;

        let upstream_server_id = state
            .server_storage
            .add_server("Upstream", &upstream.uri(), 100, MediaStreamingMode::Proxy)
            .await
            .unwrap();
        let upstream_server = state
            .server_storage
            .get_server_by_id(upstream_server_id)
            .await
            .unwrap()
            .unwrap();

        let user = state
            .user_authorization
            .get_or_create_user("MyUser", &"local-pass".into())
            .await
            .unwrap();

        state
            .user_authorization
            .add_server_mapping(
                &user.id,
                &upstream_server,
                "mappeduser",
                &"mappedpass".into(),
                None,
            )
            .await
            .unwrap();

        let existing_web_auth = Authorization {
            client: "Jellyfin Web".to_string(),
            device: "Firefox".to_string(),
            device_id: "web-device-id".to_string(),
            version: "10.10.7".to_string(),
            token: None,
        };

        state
            .user_authorization
            .store_authorization_session(
                &user.id,
                &upstream_server,
                &existing_web_auth,
                "web-upstream-token".to_string(),
                "upstream-user-id".to_string(),
                None,
            )
            .await
            .unwrap();

        let quick_connect_session = state.quick_connect.initiate(
            "androidtv-base-device-id".to_string(),
            "Chromecast".to_string(),
            "Jellyfin Android TV".to_string(),
            "0.19.7".to_string(),
        );

        let mut browser_headers = HeaderMap::new();
        browser_headers.insert(
            "authorization",
            HeaderValue::from_str(&format!(
                "MediaBrowser Client=\"Jellyfin Web\", Device=\"Firefox\", DeviceId=\"web-device-id\", Version=\"10.10.7\", Token=\"{}\"",
                user.virtual_key
            ))
            .unwrap(),
        );

        let authorize_result = handle_quick_connect_authorize(
            Query(AuthorizeQuery {
                code: quick_connect_session.code.clone(),
                user_id: None,
            }),
            axum::extract::State(state.clone()),
            browser_headers,
        )
        .await;
        assert!(authorize_result.is_ok());

        let mut tv_headers = HeaderMap::new();
        tv_headers.insert(
            "authorization",
            HeaderValue::from_static(
                "MediaBrowser Client=\"Jellyfin Android TV\", Device=\"Chromecast\", DeviceId=\"androidtv-base-device-id\", Version=\"0.19.7\"",
            ),
        );

        let auth_response = handle_authenticate_with_quick_connect(
            axum::extract::State(state.clone()),
            tv_headers,
            Json(QuickConnectAuthenticateRequest {
                secret: quick_connect_session.secret.clone(),
            }),
        )
        .await
        .unwrap()
        .1
         .0;

        assert_eq!(auth_response.access_token, user.virtual_key);

        let all_sessions = state
            .user_authorization
            .get_user_sessions(&user.id, None)
            .await
            .unwrap();
        assert_eq!(all_sessions.len(), 2, "web and TV sessions should coexist");

        let web_sessions = state
            .user_authorization
            .get_user_sessions(
                &user.id,
                Some(Device {
                    client: "Jellyfin Web".to_string(),
                    device: "Firefox".to_string(),
                    device_id: "web-device-id".to_string(),
                    version: "10.10.7".to_string(),
                }),
            )
            .await
            .unwrap();

        assert_eq!(
            web_sessions.len(),
            1,
            "existing web session should remain available"
        );
        assert_eq!(web_sessions[0].0.jellyfin_token, "web-upstream-token");
    }
}
