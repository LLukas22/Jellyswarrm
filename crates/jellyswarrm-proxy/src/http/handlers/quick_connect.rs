//! Quick Connect HTTP adapters: initiation, polling, approval and redemption.
//! Redeemed-session identity is chosen by `application::quick_connect`.
use crate::{
    application::{
        accounts::services::AuthenticationService,
        quick_connect::{self, QuickConnectError},
    },
    auth::{
        quick_connect::{QuickConnectSession, QuickConnectStorage, RedemptionError},
        UserAuthorizationService,
    },
    domain::user_id::UserId,
    http::sessions::{authentication_response, AuthenticationResponse},
    protocol::authorization::Authorization,
};
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use hyper::StatusCode;
use jellyswarrm_macros::multi_case_struct;
use serde::Deserialize;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

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

pub(crate) async fn enabled() -> Json<bool> {
    Json(true)
}

pub(crate) async fn initiate(
    State(storage): State<QuickConnectStorage>,
    headers: HeaderMap,
) -> Json<QuickConnectSession> {
    let client = Authorization::from_headers(&headers).unwrap_or_else(|| Authorization {
        client: "Unknown App".to_string(),
        device: "Unknown Device".to_string(),
        device_id: Uuid::new_v4().to_string(),
        version: "1.0.0".to_string(),
        token: None,
    });
    info!(
        "Initiating Quick Connect session for {} / {} ({})",
        client.client, client.device, client.device_id
    );
    Json(storage.initiate(
        client.device_id,
        client.device,
        client.client,
        client.version,
    ))
}

pub(crate) async fn authorize(
    Query(params): Query<AuthorizeQuery>,
    State(storage): State<QuickConnectStorage>,
    State(authorization): State<Arc<UserAuthorizationService>>,
    headers: HeaderMap,
) -> Result<Json<bool>, StatusCode> {
    let user_id = resolve_authorize_user_id(&authorization, &headers, params.user_id).await?;
    if !storage.approve(&params.code.as_str().into(), user_id.clone()) {
        return Err(StatusCode::NOT_FOUND);
    }
    info!(
        "Authorized Quick Connect code {} for user {}",
        params.code, user_id
    );
    Ok(Json(true))
}

async fn resolve_authorize_user_id(
    state: &UserAuthorizationService,
    headers: &HeaderMap,
    user_id: Option<UserId>,
) -> Result<UserId, StatusCode> {
    let token = Authorization::token_from_headers(headers).ok_or_else(|| {
        warn!("Quick Connect authorize called without a virtual token");
        StatusCode::BAD_REQUEST
    })?;

    let user = state
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

pub(crate) async fn connect(
    Query(params): Query<ConnectQuery>,
    State(storage): State<QuickConnectStorage>,
) -> Result<Json<QuickConnectSession>, StatusCode> {
    storage
        .poll(&params.secret.as_str().into())
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

pub(crate) async fn authenticate(
    State(storage): State<QuickConnectStorage>,
    State(state): State<AuthenticationService>,
    headers: HeaderMap,
    Json(request): Json<QuickConnectAuthenticateRequest>,
) -> Result<AuthenticationResponse, StatusCode> {
    let redemption = storage
        .begin_redemption(&request.secret.as_str().into())
        .map_err(|error| match error {
            RedemptionError::NotFound => StatusCode::NOT_FOUND,
            RedemptionError::NotApproved => StatusCode::UNAUTHORIZED,
        })?;
    quick_connect::redeem(&state, redemption, Authorization::from_headers(&headers))
        .await
        .map(authentication_response)
        .map_err(|error| match error {
            QuickConnectError::Unauthorized => StatusCode::UNAUTHORIZED,
            QuickConnectError::NoServers => StatusCode::NOT_FOUND,
            QuickConnectError::Storage(_) | QuickConnectError::CoordinatorStopped(_) => {
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
        config::{AppConfig, MediaStreamingMode},
        models::{AuthenticateResponse, SessionInfo, SyncPlayUserAccessType, User, UserPolicy},
        persistence::MIGRATOR,
        protocol::device::Device,
        test_support::adapters::AdapterFixture,
    };
    use axum::{extract::Query, Json};
    use hyper::http::HeaderValue;
    use sqlx::SqlitePool;
    use std::collections::HashMap;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    async fn adapter_fixture() -> AdapterFixture {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        AdapterFixture::new(pool, AppConfig::default()).await
    }

    #[tokio::test]
    async fn authorize_cannot_use_user_id_as_a_substitute_for_caller_authentication() {
        let state = adapter_fixture().await;
        assert_eq!(
            resolve_authorize_user_id(
                &state.user_authorization,
                &HeaderMap::new(),
                Some("victim".into())
            )
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
            resolve_authorize_user_id(&state.user_authorization, &headers, Some("victim".into()))
                .await
                .unwrap_err(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            resolve_authorize_user_id(&state.user_authorization, &headers, Some(user.id.clone()))
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

    #[tokio::test]
    async fn quick_connect_authentication_preserves_existing_web_session() {
        let state = adapter_fixture().await;
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

        let authorize_result = authorize(
            Query(AuthorizeQuery {
                code: quick_connect_session.code.clone(),
                user_id: None,
            }),
            axum::extract::State(state.quick_connect.clone()),
            axum::extract::State(state.user_authorization.clone()),
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

        let auth_response = authenticate(
            axum::extract::State(state.quick_connect.clone()),
            axum::extract::State(state.authentication_service().await),
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
