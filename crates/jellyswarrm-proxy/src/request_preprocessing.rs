use axum::extract::{OriginalUri, Request};

use anyhow::Result;
use axum::http;
use http_body_util::BodyExt;
use tracing::{debug, error};

use crate::models::Authorization;
use crate::processors::analyze_json;
use crate::processors::request_analyzer::{RequestAnalysisContext, RequestBodyAnalysisResult};
use crate::server_storage::Server;
use crate::url_helper::{contains_id, join_server_url, replace_id};
use crate::user_authorization_service::{AuthorizationSession, Device, User};
use crate::AppState;

// Static configuration for server resolution
static MEDIA_ID_PATH_TAGS: &[&str] = &[
    "Items",
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
    "SeriesId",
    "MediaSourceId",
    "Tag",
    "SeasonId",
    "startItemId",
    "IDs",
];

static USER_ID_PATH_TAGS: &[&str] = &["Users"];

static USER_ID_QUERY_TAGS: &[&str] = &["UserId"];

static API_KEY_QUERY_TAGS: &[&str] = &["api_key", "ApiKey"];

#[derive(Debug, Clone)]
pub enum JellyfinAuthorization {
    Authorization(Authorization),
    XMediaBrowser(String),
    ApiKey(String),
    XEmbyToken(String),
    XEmbyAuthorization(Authorization),
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
        debug!(
            "No authorization found in request! Headers: {:?}",
            request.headers()
        );
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
        let sessions = state
            .user_authorization
            .get_user_sessions(&user.id, device)
            .await?;
        if !sessions.is_empty() {
            Some(sessions)
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
    // Get the original path and query
    let mut orig_url = request.url().clone();
    debug!("Original request URL: {}", orig_url);
    // Handle user ID replacement in the path if session is available
    if let Some(session) = session {
        for &path_segment in USER_ID_PATH_TAGS {
            if let Some(user_id) = contains_id(&orig_url, path_segment) {
                debug!(
                    "Replacing user ID in path: {} -> {}",
                    user_id, session.original_user_id
                );
                orig_url = replace_id(orig_url, &user_id, &session.original_user_id);
            }
        }
    }

    // Process media IDs in the path
    for &path_segment in MEDIA_ID_PATH_TAGS {
        if let Some(media_id) = contains_id(&orig_url, path_segment) {
            if let Some(media_mapping) = state
                .media_storage
                .get_media_mapping_by_virtual(&media_id)
                .await
                .unwrap_or_default()
            {
                debug!(
                    "Replacing media ID in path: {} -> {}",
                    media_id, media_mapping.original_media_id
                );
                orig_url = replace_id(orig_url, &media_id, &media_mapping.original_media_id);
            }
        }
    }

    // Parse and modify query pairs
    let mut pairs: Vec<(String, String)> = orig_url
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    // If session is available, update the user ID and api key in the query
    if let Some(session) = session {
        for &param_name in USER_ID_QUERY_TAGS {
            if let Some(idx) = pairs
                .iter()
                .position(|(k, _)| k.eq_ignore_ascii_case(param_name))
            {
                pairs[idx].1 = session.original_user_id.clone();
            }
        }

        for &param_name in API_KEY_QUERY_TAGS {
            if let Some(idx) = pairs
                .iter()
                .position(|(k, _)| k.eq_ignore_ascii_case(param_name))
            {
                pairs[idx].1 = session.jellyfin_token.clone();
            }
        }
    }

    // Process media IDs in the query
    for &param_name in MEDIA_ID_QUERY_TAGS {
        if let Some(idx) = pairs
            .iter()
            .position(|(k, _)| k.eq_ignore_ascii_case(param_name))
        {
            let param_value = &pairs[idx].1.clone();
            if let Some(media_mapping) = state
                .media_storage
                .get_media_mapping_by_virtual(param_value)
                .await
                .unwrap_or_default()
            {
                debug!(
                    "Replacing media ID in query: {} -> {}",
                    param_value, media_mapping.original_media_id
                );
                pairs[idx].1 = media_mapping.original_media_id;
            }
        }
    }

    let path = state.remove_prefix_from_path(orig_url.path()).await;
    let mut new_url = join_server_url(&server.url, path);
    // Clear and set new query
    new_url.query_pairs_mut().clear().extend_pairs(pairs);

    // Set the new URL on the request
    *request.url_mut() = new_url;
}

pub fn apply_authorization_header(
    request: &mut reqwest::Request,
    auth: &Option<JellyfinAuthorization>,
) {
    if let Some(auth) = auth {
        match auth {
            JellyfinAuthorization::Authorization(auth) => {
                request.headers_mut().insert(
                    reqwest::header::AUTHORIZATION,
                    reqwest::header::HeaderValue::from_str(&auth.to_header_value()).unwrap(),
                );
            }
            // Map XEmbyAuthorization to Authorization header
            JellyfinAuthorization::XEmbyAuthorization(auth) => {
                request.headers_mut().insert(
                    reqwest::header::AUTHORIZATION,
                    reqwest::header::HeaderValue::from_str(&auth.to_header_value()).unwrap(),
                );
            }
            JellyfinAuthorization::XMediaBrowser(token) => {
                request.headers_mut().insert(
                    "X-MediaBrowser-Token",
                    reqwest::header::HeaderValue::from_str(token).unwrap(),
                );
            }
            JellyfinAuthorization::XEmbyToken(token) => {
                request.headers_mut().insert(
                    "X-Emby-Token",
                    reqwest::header::HeaderValue::from_str(token).unwrap(),
                );
            }
            JellyfinAuthorization::ApiKey(_) => {}
        }
    }
}

pub fn apply_host_header(request: &mut reqwest::Request, server: &Server) {
    if let Some(host) = server.url.host_str() {
        request.headers_mut().insert(
            reqwest::header::HOST,
            reqwest::header::HeaderValue::from_str(host).unwrap(),
        );
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
    let mut request_server = None;

    // Check URL paths for media IDs using the static configuration
    for &path_segment in MEDIA_ID_PATH_TAGS {
        if let Some(media_id) = contains_id(request.url(), path_segment) {
            debug!("Found {} ID in request: {}", path_segment, media_id);
            if let Some((_mapping, server)) = state
                .media_storage
                .get_media_mapping_with_server(&media_id)
                .await?
            {
                debug!(
                    "Found server for {} ID {}: {} ({})",
                    path_segment, media_id, server.name, server.url
                );
                request_server = Some(server);
                break; // Stop at first match
            } else {
                debug!("No server found for {} ID: {}", path_segment, media_id);
            }
        }
    }

    // Check query parameters using the static configuration
    if request_server.is_none() {
        for &param_name in MEDIA_ID_QUERY_TAGS {
            if let Some(param_value) = request
                .url()
                .query_pairs()
                .find(|(k, _)| k.eq_ignore_ascii_case(param_name))
                .map(|(_, v)| v.to_string())
            {
                debug!("Found {} in query: {}", param_name, param_value);
                if let Some((_mapping, server)) = state
                    .media_storage
                    .get_media_mapping_with_server(&param_value)
                    .await?
                {
                    debug!(
                        "Found server for {} {}: {} ({})",
                        param_name, param_value, server.name, server.url
                    );
                    request_server = Some(server);
                    break; // Stop at first match
                } else {
                    debug!("No server found for {} : {}", param_name, param_value);
                }
            }
        }
    }

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
            if let Some((session, server)) = sessions.iter().find(|(_, server)| {
                let request_url = request_server.url.as_str().trim_end_matches('/');
                let server_url = server.url.as_str().trim_end_matches('/');
                request_url == server_url
            }) {
                debug!("Found server in request: {}", server.url);
                return Ok((server.clone(), Some(session.clone())));
            }
        }

        let (session, server) = sessions.first().unwrap();
        return Ok((server.clone(), Some(session.clone())));
    }

    if let Some(request_server) = request_server {
        debug!("Using request server: {}", request_server.url);
        return Ok((request_server, None));
    }

    let server = state.server_storage.get_best_server().await?;
    let server = server.ok_or_else(|| anyhow::anyhow!("No server available"))?;
    Ok((server, None))
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
    let original_uri = req.extensions().get::<OriginalUri>().unwrap();

    let uri_with_host = http::uri::Builder::new()
        .scheme("http")
        .authority("localhost")
        .path_and_query(original_uri.path_and_query().unwrap().to_string())
        .build()
        .unwrap();

    // First extract parts and body separately
    let (parts, body) = req.into_parts();
    let body_bytes = body.collect().await?.to_bytes();

    let mut http_req = http::Request::from_parts(parts, reqwest::Body::from(body_bytes));
    *http_req.uri_mut() = uri_with_host;

    let rewquest_req =
        reqwest::Request::try_from(http_req).expect("http::Uri to url::Url conversion failed");

    Ok(rewquest_req)
}

fn remove_hop_by_hop_headers(headers: &mut reqwest::header::HeaderMap) {
    let hop_by_hop_headers = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailers",
        "transfer-encoding",
        "upgrade",
    ];
    for h in hop_by_hop_headers.iter() {
        headers.remove(*h);
    }
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
