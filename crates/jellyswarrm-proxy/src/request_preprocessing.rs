use axum::extract::{OriginalUri, Request};

use anyhow::{anyhow, Result};
use axum::http;
use http_body_util::BodyExt;
use std::fmt;
use tracing::{debug, error};

use crate::models::Authorization;
use crate::processors::analyze_json;
use crate::processors::request_analyzer::{RequestAnalysisContext, RequestBodyAnalysisResult};
use crate::proxy_headers::remove_hop_by_hop_headers;
use crate::server_storage::Server;
use crate::url_helper::join_server_url;
use crate::user_authorization_service::{AuthorizationSession, Device, User};
use crate::AppState;

pub struct RequestIdentity {
    pub auth: Option<JellyfinAuthorization>,
    pub user: Option<User>,
    pub device: Option<Device>,
}

pub async fn resolve_request_identity_from_headers_uri(
    headers: &http::HeaderMap,
    uri: &http::Uri,
    state: &AppState,
) -> Result<RequestIdentity> {
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let url = url::Url::parse(&format!("http://localhost{path_and_query}"))?;

    let mut request = reqwest::Request::new(reqwest::Method::GET, url);
    request.headers_mut().extend(headers.clone());

    let auth = JellyfinAuthorization::from_request(&request);
    let mut device = auth.as_ref().and_then(|a| a.get_device(request.headers()));
    if device.is_none() {
        let query_device_id = request.url().query_pairs().find_map(|(k, v)| {
            if k.eq_ignore_ascii_case("deviceid") {
                Some(v.to_string())
            } else {
                None
            }
        });
        if let Some(device_id) = query_device_id {
            let ua_device = request
                .headers()
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                .map(Device::from_useragent)
                .unwrap_or(Device {
                    client: "Unknown".to_string(),
                    device: "Unknown".to_string(),
                    device_id: device_id.clone(),
                    version: "Unknown".to_string(),
                });

            device = Some(Device {
                device_id,
                ..ua_device
            });
        }
    }
    let user = get_user_from_request(&auth, state).await?;

    Ok(RequestIdentity { auth, user, device })
}

#[derive(Clone)]
pub enum JellyfinAuthorization {
    Authorization(Authorization),
    XMediaBrowser(String),
    ApiKey(String),
    XEmbyToken(String),
    XEmbyAuthorization(Authorization),
}

impl fmt::Debug for JellyfinAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JellyfinAuthorization::Authorization(auth) => {
                f.debug_tuple("Authorization").field(auth).finish()
            }
            JellyfinAuthorization::XMediaBrowser(_) => {
                f.debug_tuple("XMediaBrowser").field(&"<redacted>").finish()
            }
            JellyfinAuthorization::ApiKey(_) => {
                f.debug_tuple("ApiKey").field(&"<redacted>").finish()
            }
            JellyfinAuthorization::XEmbyToken(_) => {
                f.debug_tuple("XEmbyToken").field(&"<redacted>").finish()
            }
            JellyfinAuthorization::XEmbyAuthorization(auth) => {
                f.debug_tuple("XEmbyAuthorization").field(auth).finish()
            }
        }
    }
}

impl JellyfinAuthorization {
    pub fn token(&self) -> Option<String> {
        match self {
            JellyfinAuthorization::Authorization(auth) => auth.token.clone(),
            JellyfinAuthorization::XMediaBrowser(token) => Some(token.clone()),
            JellyfinAuthorization::ApiKey(token) => Some(token.clone()),
            JellyfinAuthorization::XEmbyToken(token) => Some(token.clone()),
            JellyfinAuthorization::XEmbyAuthorization(auth) => auth.token.clone(),
        }
    }

    pub fn get_device(&self, headers: &http::HeaderMap) -> Option<Device> {
        match self {
            JellyfinAuthorization::Authorization(auth) => Some(Device {
                client: auth.client.clone(),
                device: auth.device.clone(),
                device_id: auth.device_id.clone(),
                version: auth.version.clone(),
            }),
            JellyfinAuthorization::XEmbyAuthorization(auth) => Some(Device {
                client: auth.client.clone(),
                device: auth.device.clone(),
                device_id: auth.device_id.clone(),
                version: auth.version.clone(),
            }),
            JellyfinAuthorization::XMediaBrowser(_) => None,
            JellyfinAuthorization::ApiKey(_) => None,
            JellyfinAuthorization::XEmbyToken(_) => {
                // Try to get device info from User-Agent header
                if let Some(user_agent) = headers.get("user-agent") {
                    if let Ok(ua_str) = user_agent.to_str() {
                        let device =
                            crate::user_authorization_service::Device::from_useragent(ua_str);
                        return Some(Device {
                            client: device.client,
                            device: device.device,
                            device_id: device.device_id,
                            version: device.version,
                        });
                    }
                }
                None
            }
        }
    }

    pub fn from_request(req: &reqwest::Request) -> Option<Self> {
        let headers = req.headers();
        if let Some(auth_header) = headers.get("authorization") {
            if let Ok(auth_str) = auth_header.to_str() {
                if let Ok(auth) = Authorization::parse(auth_str) {
                    return Some(JellyfinAuthorization::Authorization(auth));
                }
            }
        }

        if let Some(auth_header) = headers.get("x-emby-authorization") {
            if let Ok(auth_str) = auth_header.to_str() {
                if let Ok(auth) = Authorization::parse(auth_str) {
                    return Some(JellyfinAuthorization::XEmbyAuthorization(auth));
                }
            }
        }

        if let Some(token_header) = headers.get("X-MediaBrowser-Token") {
            if let Ok(token_str) = token_header.to_str() {
                return Some(JellyfinAuthorization::XMediaBrowser(token_str.to_string()));
            }
        }

        if let Some(token_header) = headers.get("x-emby-token") {
            if let Ok(token_str) = token_header.to_str() {
                return Some(JellyfinAuthorization::XEmbyToken(token_str.to_string()));
            }
        }

        if let Some(auth) = req.url().query_pairs().find_map(|(k, v)| {
            if (k == "api_key") | (k == "ApiKey") {
                Some(JellyfinAuthorization::ApiKey(v.to_string()))
            } else {
                None
            }
        }) {
            return Some(auth);
        }

        None
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct PreprocessedRequest {
    pub request: reqwest::Request,
    pub original_request: reqwest::Request,
    pub user: Option<User>,
    pub sessions: Option<Vec<(AuthorizationSession, Server)>>,
    pub server: Server,
    pub auth: Option<JellyfinAuthorization>,
    pub session: Option<AuthorizationSession>,
    pub new_auth: Option<JellyfinAuthorization>,
}

pub async fn extract_request_infos(
    req: Request,
    state: &AppState,
) -> Result<(
    reqwest::Request,
    Option<JellyfinAuthorization>,
    Option<User>,
    Option<Vec<(AuthorizationSession, Server)>>,
    Option<RequestBodyAnalysisResult>,
)> {
    let request = axum_to_reqwest(req).await?;

    let auth = JellyfinAuthorization::from_request(&request);

    if let Some(auth) = &auth {
        debug!("Extracted authorization: {:?}", auth);
    } else {
        debug!("No authorization found in request");
    }

    let device = if let Some(auth) = &auth {
        auth.get_device(request.headers())
    } else {
        None
    };

    let mut user = get_user_from_request(&auth, state).await?;

    // look into the body for information
    let request_body_result = if let Some(json) = body_to_json(&request) {
        let accumulator = RequestBodyAnalysisResult::default();
        let context = RequestAnalysisContext;
        let analysis_result = analyze_json(
            &json,
            &state.processors.request_analyzer,
            &context,
            accumulator,
        )
        .await?;
        if let Some(found_user) = analysis_result.get_user() {
            debug!("Found user in request body: {:?}", found_user);
            if user.is_none() {
                user = Some(found_user);
            }
        }

        if let Some(found_server) = analysis_result.get_server() {
            debug!("Found server in request body: {}", &found_server.name);
        }
        Some(analysis_result)
    } else {
        debug!("No JSON body found in request");
        None
    };

    let sessions = if let Some(user) = &user {
        let mut sessions = state
            .user_authorization
            .get_user_sessions(&user.id, device.clone())
            .await?;

        // ANDROID TV DEVICE-ID REBIND (intentional behavior):
        // Android TV can authenticate with a username-derived device ID and then switch to a
        // user-id-derived device ID on the very next authenticated request. Our normal session
        // lookup is strict on device ID, so the first request after login may not find a match.
        // To keep the rest of the pipeline unchanged, we do a one-time Android-TV-only rebind
        // when strict lookup returns no session, then re-run strict lookup.
        if sessions.is_empty() {
            if let Some(device) = &device {
                let rebound = state
                    .user_authorization
                    .rebind_android_tv_device_sessions_if_needed(&user.id, device)
                    .await?;

                if rebound {
                    sessions = state
                        .user_authorization
                        .get_user_sessions(&user.id, Some(device.clone()))
                        .await?;
                }
            }
        }

        // filter for online servers only
        let mut filtered_sessions: Vec<(AuthorizationSession, Server)> =
            Vec::with_capacity(sessions.len());
        for (session, server) in sessions {
            if state
                .server_storage
                .server_status(server.id)
                .await
                .is_healthy()
            {
                filtered_sessions.push((session, server));
            }
        }

        if !filtered_sessions.is_empty() {
            Some(filtered_sessions)
        } else {
            None
        }
    } else {
        None
    };

    Ok((request, auth, user, sessions, request_body_result))
}

pub async fn preprocess_request(req: Request, state: &AppState) -> Result<PreprocessedRequest> {
    debug!("Preprocessing request: {:?}", req.uri());
    let (mut request, auth, user, sessions, request_body_result) =
        extract_request_infos(req, state).await?;
    let original_request = request
        .try_clone()
        .ok_or_else(|| anyhow!("failed to clone preprocessed request body"))?;

    let (server, session) =
        resolve_server(&sessions, &request_body_result, state, &request).await?;

    let new_auth = remap_authorization(&auth, &session).await?;

    apply_to_request(&mut request, &server, &session, &new_auth, state).await;

    Ok(PreprocessedRequest {
        request,
        original_request,
        user,
        sessions,
        server,
        auth,
        session,
        new_auth,
    })
}

pub async fn apply_to_request(
    request: &mut reqwest::Request,
    server: &Server,
    session: &Option<AuthorizationSession>,
    auth: &Option<JellyfinAuthorization>,
    state: &AppState,
) {
    remove_hop_by_hop_headers(request.headers_mut());

    apply_host_header(request, server);

    apply_authorization_header(request, auth);

    apply_new_target_uri(request, server, session, state).await;
}

pub async fn apply_new_target_uri(
    request: &mut reqwest::Request,
    server: &Server,
    session: &Option<AuthorizationSession>,
    state: &AppState,
) {
    let mut orig_url = request.url().clone();
    debug!("Original request URL: {}", orig_url);

    state
        .processors
        .url_processor
        .client_to_server_url(&mut orig_url, session)
        .await;

    let path = state.remove_prefix_from_path(orig_url.path()).await;
    let mut new_url = join_server_url(&server.url, path);
    new_url.set_query(orig_url.query());

    *request.url_mut() = new_url;
}

pub fn apply_authorization_header(
    request: &mut reqwest::Request,
    auth: &Option<JellyfinAuthorization>,
) {
    //Remove stale auth headers
    let headers = request.headers_mut();
    headers.remove(reqwest::header::AUTHORIZATION);
    headers.remove("X-Emby-Authorization");
    headers.remove("X-Emby-Token");
    headers.remove("X-MediaBrowser-Token");

    if let Some(auth) = auth {
        match auth {
            JellyfinAuthorization::Authorization(auth) => {
                if let Ok(value) = reqwest::header::HeaderValue::from_str(&auth.to_header_value()) {
                    request
                        .headers_mut()
                        .insert(reqwest::header::AUTHORIZATION, value);
                }
            }
            // Map XEmbyAuthorization to Authorization header
            JellyfinAuthorization::XEmbyAuthorization(auth) => {
                if let Ok(value) = reqwest::header::HeaderValue::from_str(&auth.to_header_value()) {
                    request
                        .headers_mut()
                        .insert(reqwest::header::AUTHORIZATION, value);
                }
            }
            JellyfinAuthorization::XMediaBrowser(token) => {
                if let Ok(value) = reqwest::header::HeaderValue::from_str(token) {
                    request.headers_mut().insert("X-MediaBrowser-Token", value);
                }
            }
            JellyfinAuthorization::XEmbyToken(token) => {
                if let Ok(value) = reqwest::header::HeaderValue::from_str(token) {
                    request.headers_mut().insert("X-Emby-Token", value);
                }
            }
            JellyfinAuthorization::ApiKey(_) => {}
        }
    }
}

pub fn apply_host_header(request: &mut reqwest::Request, server: &Server) {
    if let Some(host) = server.url.host_str() {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(host) {
            request.headers_mut().insert(reqwest::header::HOST, value);
        }
    }
}

pub async fn remap_authorization(
    auth: &Option<JellyfinAuthorization>,
    session: &Option<AuthorizationSession>,
) -> Result<Option<JellyfinAuthorization>> {
    let Some(auth) = auth else {
        return Ok(None);
    };

    let remapped_session = if let Some(session) = session {
        match auth {
            JellyfinAuthorization::Authorization(_) => Some(JellyfinAuthorization::Authorization(
                session.to_authorization(),
            )),
            JellyfinAuthorization::XMediaBrowser(_) => {
                let token = session.jellyfin_token.clone();
                Some(JellyfinAuthorization::XMediaBrowser(token))
            }
            JellyfinAuthorization::ApiKey(_) => {
                let token = session.jellyfin_token.clone();
                Some(JellyfinAuthorization::ApiKey(token))
            }
            JellyfinAuthorization::XEmbyToken(_) => Some(JellyfinAuthorization::Authorization(
                session.to_authorization(),
            )),
            JellyfinAuthorization::XEmbyAuthorization(_) => Some(
                JellyfinAuthorization::Authorization(session.to_authorization()),
            ),
        }
    } else {
        None
    };
    debug!("Remapped authorization to: {:?}", remapped_session);
    Ok(remapped_session)
}
pub async fn resolve_server(
    sessions: &Option<Vec<(AuthorizationSession, Server)>>,
    request_body_result: &Option<RequestBodyAnalysisResult>,
    state: &AppState,
    request: &reqwest::Request,
) -> Result<(Server, Option<AuthorizationSession>)> {
    let mut request_server = server_from_request_media_ids(state, request).await?;

    if request_server.is_none() {
        if let Some(request_body_result) = request_body_result {
            if let Some(found_server) = request_body_result.get_server() {
                debug!(
                    "Using server found in request body analysis: {} ({})",
                    found_server.name, found_server.url
                );
                request_server = Some(found_server);
            }
        }
    }

    if let Some(sessions) = sessions {
        if let Some(request_server) = request_server {
            if let Some((session, server)) = sessions
                .iter()
                .find(|(_, server)| request_server.id == server.id)
            {
                debug!("Found server in request: {}", server.url);
                return Ok((server.clone(), Some(session.clone())));
            }
        }

        let Some((session, server)) = sessions.first() else {
            return Err(anyhow!("no authorization sessions available"));
        };
        return Ok((server.clone(), Some(session.clone())));
    }

    if let Some(request_server) = request_server {
        debug!("Using request server: {}", request_server.url);
        return Ok((request_server, None));
    }

    let server = state.server_storage.get_best_server().await?;
    let server = server.ok_or_else(|| anyhow!("No server available"))?;
    Ok((server, None))
}

async fn server_from_request_media_ids(
    state: &AppState,
    request: &reqwest::Request,
) -> Result<Option<Server>> {
    state
        .processors
        .url_processor
        .server_from_client_url(request.url())
        .await
}

pub async fn get_user_from_request(
    auth: &Option<JellyfinAuthorization>,
    state: &AppState,
) -> Result<Option<User>> {
    let Some(auth) = auth else {
        return Ok(None);
    };

    let Some(token) = auth.token() else {
        // No token, return None
        return Ok(None);
    };

    let user = state
        .user_authorization
        .get_user_by_virtual_key(&token)
        .await?;

    Ok(user)
}

pub async fn axum_to_reqwest(req: Request) -> Result<reqwest::Request> {
    let original_uri = req
        .extensions()
        .get::<OriginalUri>()
        .ok_or_else(|| anyhow!("missing original request URI"))?;
    let path_and_query = original_uri
        .path_and_query()
        .ok_or_else(|| anyhow!("missing request path and query"))?;

    let uri_with_host = http::uri::Builder::new()
        .scheme("http")
        .authority("localhost")
        .path_and_query(path_and_query.to_string())
        .build()?;

    // First extract parts and body separately
    let (parts, body) = req.into_parts();
    let body_bytes = body.collect().await?.to_bytes();

    let mut http_req = http::Request::from_parts(parts, reqwest::Body::from(body_bytes));
    *http_req.uri_mut() = uri_with_host;

    let rewquest_req = reqwest::Request::try_from(http_req)?;

    Ok(rewquest_req)
}

/// Try to parse the body of a reqwest::Request into serde_json::Value
pub fn body_to_json(request: &reqwest::Request) -> Option<serde_json::Value> {
    if let Some(content_type) = request
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        if content_type.contains("application/json") {
            if let Some(body) = request.body() {
                // Clone the body bytes since we need to read them
                let body_bytes = body.as_bytes().unwrap_or(&[]);
                if !body_bytes.is_empty() {
                    match serde_json::from_slice(body_bytes) {
                        Ok(json_value) => return Some(json_value),
                        Err(e) => {
                            error!("Failed to parse JSON body: {}", e);
                            return None;
                        }
                    }
                } else {
                    debug!("Request body is empty");
                    return None;
                }
            }
            None
        } else {
            None
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processors::url_processor::{
        matches_case_insensitive, MEDIA_ID_PATH_TAGS, MEDIA_ID_QUERY_TAGS,
    };
    use crate::url_helper::contains_id;

    #[test]
    fn media_id_tags_cover_audio_paths_and_item_id_queries() {
        let audio_id = "11111111111111111111111111111111";
        let audio_url = url::Url::parse(&format!(
            "http://localhost/Audio/{audio_id}/universal?ItemId=22222222222222222222222222222222"
        ))
        .unwrap();

        let matched_path_id = MEDIA_ID_PATH_TAGS
            .iter()
            .find_map(|path_segment| contains_id(&audio_url, path_segment));

        assert_eq!(matched_path_id.as_deref(), Some(audio_id));
        assert!(matches_case_insensitive("ItemId", MEDIA_ID_QUERY_TAGS));
    }

    use crate::config::{AppConfig, MIGRATOR};
    use crate::handlers::quick_connect::QuickConnectStorage;
    use crate::media_storage_service::MediaStorageService;
    use crate::merged_library_service::MergedLibraryService;
    use crate::server_storage::ServerStorageService;
    use crate::session_storage::SessionStorage;
    use crate::user_authorization_service::UserAuthorizationService;
    use crate::{DataContext, ProxyProcessors};
    use sqlx::SqlitePool;
    use std::sync::Arc;

    async fn create_test_app_state() -> AppState {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        let data_context = DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(ServerStorageService::new(pool.clone())),
            media_storage: Arc::new(MediaStorageService::new(pool.clone())),
            merged_library_service: Arc::new(MergedLibraryService::new(pool)),
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
    async fn resolve_identity_ignores_valid_userid_path_segment_without_auth() {
        let state = create_test_app_state().await;
        let victim = state
            .user_authorization
            .get_or_create_user("victim", &"password123".into())
            .await
            .unwrap();

        let headers = http::HeaderMap::new();
        let uri: http::Uri = format!("/Users/{}", victim.id).parse().unwrap();

        let identity = resolve_request_identity_from_headers_uri(&headers, &uri, &state)
            .await
            .unwrap();

        assert!(identity.user.is_none());
    }

    #[tokio::test]
    async fn resolve_identity_ignores_valid_userid_query_on_user_views_without_auth() {
        let state = create_test_app_state().await;
        let victim = state
            .user_authorization
            .get_or_create_user("victim", &"password123".into())
            .await
            .unwrap();

        let headers = http::HeaderMap::new();
        let uri: http::Uri = format!("/UserViews?userId={}", victim.id).parse().unwrap();

        let identity = resolve_request_identity_from_headers_uri(&headers, &uri, &state)
            .await
            .unwrap();

        assert!(identity.user.is_none());
    }

    #[tokio::test]
    async fn resolve_identity_ignores_valid_userid_query_on_user_items_resume_without_auth() {
        let state = create_test_app_state().await;
        let victim = state
            .user_authorization
            .get_or_create_user("victim", &"password123".into())
            .await
            .unwrap();

        let headers = http::HeaderMap::new();
        let uri: http::Uri = format!("/UserItems/Resume?userId={}", victim.id)
            .parse()
            .unwrap();

        let identity = resolve_request_identity_from_headers_uri(&headers, &uri, &state)
            .await
            .unwrap();

        assert!(identity.user.is_none());
    }

    #[tokio::test]
    async fn resolve_identity_treats_malformed_authorization_header_as_unauthenticated() {
        let state = create_test_app_state().await;
        let victim = state
            .user_authorization
            .get_or_create_user("victim", &"password123".into())
            .await
            .unwrap();

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer sometoken"),
        );
        let uri: http::Uri = format!("/UserViews?userId={}", victim.id).parse().unwrap();

        let identity = resolve_request_identity_from_headers_uri(&headers, &uri, &state)
            .await
            .unwrap();

        assert!(identity.auth.is_none());
        assert!(identity.user.is_none());
    }

    #[tokio::test]
    async fn resolve_identity_resolves_authenticated_user_ignoring_different_userid_in_url() {
        let state = create_test_app_state().await;
        let caller = state
            .user_authorization
            .get_or_create_user("caller", &"password123".into())
            .await
            .unwrap();
        let other = state
            .user_authorization
            .get_or_create_user("other", &"password456".into())
            .await
            .unwrap();

        let auth_header = Authorization {
            client: "Test".to_string(),
            device: "Test".to_string(),
            device_id: "test-device".to_string(),
            version: "1.0.0".to_string(),
            token: Some(caller.virtual_key.clone()),
        }
        .to_header_value();

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&auth_header).unwrap(),
        );
        let uri: http::Uri = format!("/UserViews?userId={}", other.id).parse().unwrap();

        let identity = resolve_request_identity_from_headers_uri(&headers, &uri, &state)
            .await
            .unwrap();

        assert_eq!(identity.user.unwrap().id, caller.id);
    }
}
