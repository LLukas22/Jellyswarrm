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
use crate::url_helper::{contains_id, join_server_url, replace_id};
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
    let user = get_user_from_request(&request, &auth, state).await?;

    Ok(RequestIdentity { auth, user, device })
}

// Static configuration for server resolution
static MEDIA_ID_PATH_TAGS: &[&str] = &[
    "Items",
    "Audio",
    "Shows",
    "Videos",
    "PlayedItems",
    "FavoriteItems",
    "MediaSegments",
    "PlayingItems",
    "Recordings",
    "Channels",
    "Programs",
    "SeriesTimers",
    "Timers",
    "UserFavoriteItems",
    "UserItems",
    "UserPlayedItems",
];

static MEDIA_ID_QUERY_TAGS: &[&str] = &[
    "ParentId",
    "ItemId",
    "SeriesId",
    "MediaSourceId",
    "Tag",
    "SeasonId",
    "startItemId",
    "IDs",
    "PersonIds",
];

static USER_ID_PATH_TAGS: &[&str] = &["Users"];

static USER_ID_QUERY_TAGS: &[&str] = &["UserId"];

static API_KEY_QUERY_TAGS: &[&str] = &["api_key", "ApiKey"];

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
    pub original_request: Option<reqwest::Request>,
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

    let mut user = get_user_from_request(&request, &auth, state).await?;

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
    let original_request = request.try_clone();

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

    replace_user_ids_in_path(&mut orig_url, session);
    replace_media_ids_in_path(&mut orig_url, state).await;

    let mut pairs: Vec<(String, String)> = orig_url
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    replace_session_query_values(&mut pairs, session);
    replace_media_ids_in_query(&mut pairs, state).await;

    let path = state.remove_prefix_from_path(orig_url.path()).await;
    let mut new_url = join_server_url(&server.url, path);
    new_url.query_pairs_mut().clear().extend_pairs(pairs);

    *request.url_mut() = new_url;
}

fn replace_user_ids_in_path(orig_url: &mut url::Url, session: &Option<AuthorizationSession>) {
    let Some(session) = session else {
        return;
    };

    for &path_segment in USER_ID_PATH_TAGS {
        if let Some(user_id) = contains_id(orig_url, path_segment) {
            debug!(
                "Replacing user ID in path: {} -> {}",
                user_id, session.original_user_id
            );
            *orig_url = replace_id(orig_url.clone(), &user_id, &session.original_user_id);
        }
    }
}

async fn replace_media_ids_in_path(orig_url: &mut url::Url, state: &AppState) {
    for &path_segment in MEDIA_ID_PATH_TAGS {
        if let Some(media_id) = contains_id(orig_url, path_segment) {
            let direct = state
                .media_storage
                .get_media_mapping_by_virtual(&media_id)
                .await
                .unwrap_or_default();

            // For merged library folders the virtual_id is not in media_mappings directly;
            // resolve through the first member instead.
            let media_mapping = match direct {
                Some(m) => Some(m),
                None => {
                    if let Ok(Some(rep_id)) = state
                        .merged_library_service
                        .get_first_member_virtual_id(&media_id)
                        .await
                    {
                        state
                            .media_storage
                            .get_media_mapping_by_virtual(&rep_id)
                            .await
                            .unwrap_or_default()
                    } else {
                        None
                    }
                }
            };

            if let Some(media_mapping) = media_mapping {
                debug!(
                    "Replacing media ID in path: {} -> {}",
                    media_id, media_mapping.original_media_id
                );
                *orig_url = replace_id(
                    orig_url.clone(),
                    &media_id,
                    &media_mapping.original_media_id,
                );
            }
        }
    }
}

fn replace_session_query_values(
    pairs: &mut [(String, String)],
    session: &Option<AuthorizationSession>,
) {
    let Some(session) = session else {
        return;
    };

    for (name, value) in pairs {
        if matches_case_insensitive(name, USER_ID_QUERY_TAGS) {
            *value = session.original_user_id.clone();
        } else if matches_case_insensitive(name, API_KEY_QUERY_TAGS) {
            *value = session.jellyfin_token.clone();
        }
    }
}

async fn replace_media_ids_in_query(pairs: &mut [(String, String)], state: &AppState) {
    for (name, value) in pairs {
        if !matches_case_insensitive(name, MEDIA_ID_QUERY_TAGS) {
            continue;
        }

        if let Some(resolved_value) = resolve_media_id_list(value, state).await {
            *value = resolved_value;
        }
    }
}

async fn resolve_media_id_list(value: &str, state: &AppState) -> Option<String> {
    let mut changed = false;
    let mut resolved_ids = Vec::new();

    for raw_id in value.split(',') {
        let trimmed = raw_id.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(media_mapping) = state
            .media_storage
            .get_media_mapping_by_virtual(trimmed)
            .await
            .unwrap_or_default()
        {
            debug!(
                "Replacing media ID in query: {} -> {}",
                trimmed, media_mapping.original_media_id
            );
            resolved_ids.push(media_mapping.original_media_id);
            changed = true;
        } else {
            resolved_ids.push(trimmed.to_string());
        }
    }

    changed.then(|| resolved_ids.join(","))
}

fn matches_case_insensitive(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
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
    if let Some(server) = server_from_path_media_ids(state, request.url()).await? {
        return Ok(Some(server));
    }

    server_from_query_media_ids(state, request.url()).await
}

async fn server_from_path_media_ids(state: &AppState, url: &url::Url) -> Result<Option<Server>> {
    for &path_segment in MEDIA_ID_PATH_TAGS {
        if let Some(media_id) = contains_id(url, path_segment) {
            debug!("Found {} ID in request: {}", path_segment, media_id);
            if let Some(server) = server_from_virtual_media_id(state, &media_id).await? {
                debug!(
                    "Found server for {} ID {}: {} ({})",
                    path_segment, media_id, server.name, server.url
                );
                return Ok(Some(server));
            }
            debug!("No server found for {} ID: {}", path_segment, media_id);
        }
    }

    Ok(None)
}

async fn server_from_query_media_ids(state: &AppState, url: &url::Url) -> Result<Option<Server>> {
    for (param_name, param_value) in url.query_pairs() {
        if !matches_case_insensitive(&param_name, MEDIA_ID_QUERY_TAGS) {
            continue;
        }

        debug!("Found {} in query: {}", param_name, param_value);
        for raw_id in param_value.split(',') {
            let media_id = raw_id.trim();
            if media_id.is_empty() {
                continue;
            }

            if let Some(server) = server_from_virtual_media_id(state, media_id).await? {
                debug!(
                    "Found server for {} {}: {} ({})",
                    param_name, media_id, server.name, server.url
                );
                return Ok(Some(server));
            }
            debug!("No server found for {}: {}", param_name, media_id);
        }
    }

    Ok(None)
}

async fn server_from_virtual_media_id(state: &AppState, media_id: &str) -> Result<Option<Server>> {
    if let Some((_mapping, server)) = state
        .media_storage
        .get_media_mapping_with_server(media_id)
        .await?
    {
        return Ok(Some(server));
    }

    let Some(member_media_id) = state
        .merged_library_service
        .get_first_member_virtual_id(media_id)
        .await?
    else {
        return Ok(None);
    };

    Ok(state
        .media_storage
        .get_media_mapping_with_server(&member_media_id)
        .await?
        .map(|(_mapping, server)| server))
}

pub async fn get_user_from_request(
    request: &reqwest::Request,
    auth: &Option<JellyfinAuthorization>,
    state: &AppState,
) -> Result<Option<User>> {
    let Some(auth) = auth else {
        // No auth, check for user ID in path
        for &path_segment in USER_ID_PATH_TAGS {
            if let Some(user_id) = contains_id(request.url(), path_segment) {
                debug!("Found {} ID in request: {}", path_segment, user_id);
                let user = state.user_authorization.get_user_by_id(&user_id).await?;
                return Ok(user);
            }
        }

        // If that fails, check query parameters
        for &param_name in USER_ID_QUERY_TAGS {
            if let Some(param_value) = request
                .url()
                .query_pairs()
                .find(|(k, _)| k.eq_ignore_ascii_case(param_name))
                .map(|(_, v)| v.to_string())
            {
                debug!("Found {} in query: {}", param_name, param_value);
                let user = state
                    .user_authorization
                    .get_user_by_id(&param_value)
                    .await?;
                return Ok(user);
            }
        }
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
}
