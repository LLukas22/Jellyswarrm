use axum::extract::{OriginalUri, Request};

use anyhow::{anyhow, Result};
use axum::http;
use http_body_util::BodyExt;
use std::fmt;
use tracing::{debug, error};

use crate::models::Authorization;
use crate::processors::analyze_json;
use crate::processors::request_analyzer::{
    PlaybackSessionAction, RequestAnalysisContext, RequestBodyAnalysisResult,
};
use crate::proxy_headers::remove_hop_by_hop_headers;
use crate::server_storage::Server;
use crate::session_storage::PlaybackSession;
use crate::url_helper::join_server_url;
use crate::user_authorization_service::{AuthorizationSession, Device, User};
use crate::virtual_library_service::{compare_virtual_library_routes, VirtualLibraryAccessScope};
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
    pub fn token_ref(&self) -> Option<&str> {
        match self {
            JellyfinAuthorization::Authorization(auth) => auth.token.as_deref(),
            JellyfinAuthorization::XMediaBrowser(token)
            | JellyfinAuthorization::ApiKey(token)
            | JellyfinAuthorization::XEmbyToken(token) => Some(token),
            JellyfinAuthorization::XEmbyAuthorization(auth) => auth.token.as_deref(),
        }
    }

    pub fn token(&self) -> Option<String> {
        self.token_ref().map(str::to_string)
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
    pub access_scope: Option<VirtualLibraryAccessScope>,
    pub server_matched_request: bool,
    pub pending_playback_session_update: Option<PendingPlaybackSessionUpdate>,
}

#[derive(Debug, Clone)]
pub struct PendingPlaybackSessionUpdate {
    pub action: PlaybackSessionAction,
    pub session: PlaybackSession,
}

pub async fn extract_request_infos(
    req: Request,
    state: &AppState,
    playback_session_action: Option<PlaybackSessionAction>,
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
    let authenticated_user_id = user.as_ref().map(|user| user.id.clone());

    // look into the body for information
    let request_body_result = if let Some(json) = body_to_json(&request) {
        let accumulator = RequestBodyAnalysisResult::default();
        let context = RequestAnalysisContext {
            authenticated_user_id: authenticated_user_id.clone(),
            playback_session_action,
        };
        let analysis_result = analyze_json(
            &json,
            &state.processors.request_analyzer,
            &context,
            accumulator,
        )
        .await?;
        if playback_session_action.is_some()
            && (analysis_result.requested_play_item_id.is_none() || authenticated_user_id.is_none())
        {
            return Err(anyhow!(
                "playback report requires an authenticated user and item"
            ));
        }
        if let Some(found_user) = analysis_result.get_user() {
            debug!("Found user in request body: {:?}", found_user);
            if user.is_none() && auth.is_none() {
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
    if playback_session_action.is_some() && request_body_result.is_none() {
        return Err(anyhow!("playback session report requires a JSON body"));
    }

    let sessions = if auth.is_none() {
        None
    } else if let Some(user) = &user {
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
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let playback_session_action = playback_session_action(&method, &path, state).await;
    let (mut request, auth, user, sessions, request_body_result) =
        extract_request_infos(req, state, playback_session_action).await?;
    let original_request = request
        .try_clone()
        .ok_or_else(|| anyhow!("failed to clone preprocessed request body"))?;
    let access_scope_user_id = sessions
        .as_ref()
        .and_then(|sessions| sessions.first())
        .map(|(session, _server)| session.user_id.clone())
        .or_else(|| user.as_ref().map(|user| user.id.clone()));
    let access_scope = access_scope_user_id.map(|user_id| {
        VirtualLibraryAccessScope::new(
            user_id,
            sessions
                .as_ref()
                .map(|sessions| sessions.iter().map(|(_session, server)| server.id))
                .into_iter()
                .flatten(),
        )
    });

    let (server, session, server_matched_request) = if playback_session_action.is_some() {
        resolve_playback_report_server(
            &sessions,
            request_body_result
                .as_ref()
                .ok_or_else(|| anyhow!("missing playback report"))?,
            state,
        )
        .await?
    } else {
        resolve_server(
            &sessions,
            &request_body_result,
            state,
            &request,
            access_scope.as_ref(),
        )
        .await?
    };
    let pending_playback_session_update = match playback_session_action {
        Some(PlaybackSessionAction::Start) => request_body_result.as_ref().and_then(|result| {
            Some(PendingPlaybackSessionUpdate {
                action: PlaybackSessionAction::Start,
                session: PlaybackSession {
                    session_id: result.requested_play_session_id.clone()?,
                    item_id: result.requested_play_item_id.clone()?,
                    user_id: user.as_ref()?.id.clone(),
                    server_id: server.id,
                },
            })
        }),
        Some(action @ (PlaybackSessionAction::Refresh | PlaybackSessionAction::Remove)) => {
            request_body_result.as_ref().and_then(|result| {
                Some(PendingPlaybackSessionUpdate {
                    action,
                    session: result.authoritative_play_session.clone()?,
                })
            })
        }
        None => None,
    };

    let new_auth = remap_authorization(&auth, &session).await?;

    apply_to_request(
        &mut request,
        &server,
        &session,
        &new_auth,
        state,
        access_scope.as_ref(),
    )
    .await;

    Ok(PreprocessedRequest {
        request,
        original_request,
        user,
        sessions,
        server,
        auth,
        session,
        new_auth,
        access_scope,
        server_matched_request,
        pending_playback_session_update,
    })
}

async fn playback_session_action(
    method: &http::Method,
    request_path: &str,
    state: &AppState,
) -> Option<PlaybackSessionAction> {
    if method != http::Method::POST {
        return None;
    }

    let path = state.remove_prefix_from_path(request_path).await;
    let path = path.trim_end_matches('/');
    if path.eq_ignore_ascii_case("/Sessions/Playing") {
        Some(PlaybackSessionAction::Start)
    } else if path.eq_ignore_ascii_case("/Sessions/Playing/Progress") {
        Some(PlaybackSessionAction::Refresh)
    } else if path.eq_ignore_ascii_case("/Sessions/Playing/Stopped") {
        Some(PlaybackSessionAction::Remove)
    } else {
        None
    }
}

pub async fn apply_to_request(
    request: &mut reqwest::Request,
    server: &Server,
    session: &Option<AuthorizationSession>,
    auth: &Option<JellyfinAuthorization>,
    state: &AppState,
    access_scope: Option<&VirtualLibraryAccessScope>,
) {
    remove_hop_by_hop_headers(request.headers_mut());

    apply_host_header(request, server);

    apply_authorization_header(request, auth);

    apply_new_target_uri(request, server, session, state, access_scope).await;
}

pub async fn apply_new_target_uri(
    request: &mut reqwest::Request,
    server: &Server,
    session: &Option<AuthorizationSession>,
    state: &AppState,
    access_scope: Option<&VirtualLibraryAccessScope>,
) {
    let mut orig_url = request.url().clone();
    debug!("Original request URL: {}", orig_url);

    state
        .processors
        .url_processor
        .client_to_server_url(&mut orig_url, session, access_scope, Some(server.id))
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
// Reports must not use the generic best-server fallback: a wrong destination can
// update another copy's playback state. Only top-level item/source fields route them.
async fn resolve_playback_report_server(
    sessions: &Option<Vec<(AuthorizationSession, Server)>>,
    analysis: &RequestBodyAnalysisResult,
    state: &AppState,
) -> Result<(Server, Option<AuthorizationSession>, bool)> {
    let sessions = sessions
        .as_ref()
        .ok_or_else(|| anyhow!("no authorization sessions available"))?;
    let item_id = analysis
        .requested_play_item_id
        .as_deref()
        .ok_or_else(|| anyhow!("playback report requires an item"))?;
    let mut candidates =
        if let Some(group) = state.media_storage.get_media_version_group(item_id).await? {
            if let Some(source_id) = analysis.requested_play_source_id.as_deref() {
                vec![
                    state
                        .media_storage
                        .get_media_version_source_route(group.id, source_id)
                        .await?
                        .ok_or_else(|| anyhow!("media source does not belong to playback item"))?
                        .member_mapping
                        .server_id,
                ]
            } else {
                state
                    .media_storage
                    .get_media_version_members_by_virtual_id(item_id)
                    .await?
                    .into_iter()
                    .map(|member| member.server.id)
                    .collect()
            }
        } else {
            let (_, server) = state
                .media_storage
                .get_media_mapping_with_server(item_id)
                .await?
                .ok_or_else(|| anyhow!("unknown playback item"))?;
            if let Some(source_id) = analysis.requested_play_source_id.as_deref() {
                let (_, source_server) = state
                    .media_storage
                    .get_media_mapping_with_server(source_id)
                    .await?
                    .ok_or_else(|| anyhow!("unknown playback source"))?;
                if source_server.id != server.id {
                    return Err(anyhow!("conflicting playback item and source servers"));
                }
            }
            vec![server.id]
        };
    if let Some(play_session) = &analysis.authoritative_play_session {
        if !candidates.contains(&play_session.server_id) {
            return Err(anyhow!(
                "conflicting playback session and item/source authority"
            ));
        }
        candidates.retain(|id| *id == play_session.server_id);
    }
    candidates.retain(|id| sessions.iter().any(|(_, server)| server.id == *id));
    if candidates.len() != 1 {
        return Err(anyhow!(
            "playback item/source route is unavailable or ambiguous"
        ));
    }
    let (session, server) = sessions
        .iter()
        .find(|(_, server)| server.id == candidates[0])
        .ok_or_else(|| anyhow!("playback server is unavailable"))?;
    Ok((server.clone(), Some(session.clone()), true))
}

pub async fn resolve_server(
    sessions: &Option<Vec<(AuthorizationSession, Server)>>,
    request_body_result: &Option<RequestBodyAnalysisResult>,
    state: &AppState,
    request: &reqwest::Request,
    access_scope: Option<&VirtualLibraryAccessScope>,
) -> Result<(Server, Option<AuthorizationSession>, bool)> {
    if let Some(play_session) = request_body_result
        .as_ref()
        .and_then(|result| result.authoritative_play_session.as_ref())
    {
        let server = state
            .server_storage
            .get_server_by_id(play_session.server_id)
            .await?
            .ok_or_else(|| anyhow!("playback session server no longer exists"))?;
        let (session, _) = sessions
            .as_ref()
            .and_then(|sessions| {
                sessions
                    .iter()
                    .find(|(_session, candidate)| candidate.id == server.id)
            })
            .ok_or_else(|| anyhow!("playback session server is unavailable"))?;
        return Ok((server, Some(session.clone()), true));
    }

    let mut request_server = server_from_request_media_ids(state, request, access_scope).await?;

    if request_server.is_none() {
        if let Some(request_body_result) = request_body_result {
            if let Some(found_server) = request_body_result.get_server() {
                if access_scope.is_none_or(|scope| scope.allows(found_server.id)) {
                    debug!(
                        "Using server found in request body analysis: {} ({})",
                        found_server.name, found_server.url
                    );
                    request_server = Some(found_server);
                }
            }
        }
    }

    if request_server.is_none() {
        request_server =
            server_from_body_media_aggregate_ids(state, request_body_result.as_ref(), access_scope)
                .await?;
    }

    if let Some(sessions) = sessions {
        if let Some(request_server) = request_server {
            if let Some((session, server)) = sessions
                .iter()
                .find(|(_, server)| request_server.id == server.id)
            {
                debug!("Found server in request: {}", server.url);
                return Ok((server.clone(), Some(session.clone()), true));
            }
        }

        let Some((session, server)) = sessions.first() else {
            return Err(anyhow!("no authorization sessions available"));
        };
        return Ok((server.clone(), Some(session.clone()), false));
    }

    if access_scope.is_some() {
        return Err(anyhow!("no authorization sessions available"));
    }

    if let Some(request_server) = request_server {
        debug!("Using request server: {}", request_server.url);
        return Ok((request_server, None, true));
    }

    let server = state.server_storage.get_best_server().await?;
    let server = server.ok_or_else(|| anyhow!("No server available"))?;
    Ok((server, None, false))
}

async fn server_from_body_media_aggregate_ids(
    state: &AppState,
    analysis: Option<&RequestBodyAnalysisResult>,
    access_scope: Option<&VirtualLibraryAccessScope>,
) -> Result<Option<Server>> {
    let Some(analysis) = analysis else {
        return Ok(None);
    };
    for media_id in &analysis.found_ids {
        let members = state
            .media_storage
            .get_media_version_members_by_virtual_id(media_id)
            .await?;
        let mut healthy_members = Vec::new();
        for member in members {
            if access_scope.is_none_or(|scope| scope.allows(member.server.id))
                && state
                    .server_storage
                    .server_status(member.server.id)
                    .await
                    .is_healthy()
            {
                healthy_members.push(member);
            }
        }
        if let Some(member) = healthy_members.into_iter().max_by(|left, right| {
            compare_virtual_library_routes(
                &left.server,
                &left.mapping.original_media_id,
                &right.server,
                &right.mapping.original_media_id,
            )
        }) {
            return Ok(Some(member.server));
        }
    }
    Ok(None)
}

async fn server_from_request_media_ids(
    state: &AppState,
    request: &reqwest::Request,
    access_scope: Option<&VirtualLibraryAccessScope>,
) -> Result<Option<Server>> {
    state
        .processors
        .url_processor
        .server_from_client_url(request.url(), access_scope)
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

    let user = state.user_authorization.get_user_by_token(&token).await?;

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
        assert!(matches_case_insensitive("AlbumId", MEDIA_ID_QUERY_TAGS));
        assert!(matches_case_insensitive("AlbumIds", MEDIA_ID_QUERY_TAGS));
        assert!(matches_case_insensitive("ArtistIds", MEDIA_ID_QUERY_TAGS));
        assert!(matches_case_insensitive(
            "ContributingArtistIds",
            MEDIA_ID_QUERY_TAGS
        ));
        assert!(matches_case_insensitive(
            "AlbumArtistIds",
            MEDIA_ID_QUERY_TAGS
        ));
        assert!(matches_case_insensitive(
            "ExcludeArtistIds",
            MEDIA_ID_QUERY_TAGS
        ));
    }

    use crate::config::{AppConfig, MIGRATOR};
    use crate::handlers::quick_connect::QuickConnectStorage;
    use crate::media_storage_service::MediaStorageService;
    use crate::server_id::ServerId;
    use crate::server_storage::ServerStorageService;
    use crate::session_storage::SessionStorage;
    use crate::user_authorization_service::UserAuthorizationService;
    use crate::virtual_library_service::VirtualLibraryService;
    use crate::{DataContext, ProxyProcessors};
    use sqlx::SqlitePool;
    use std::sync::Arc;

    async fn create_test_app_state() -> AppState {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());

        let data_context = DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(server_storage.clone()),
            media_storage: Arc::new(media_storage.clone()),
            virtual_library_service: Arc::new(VirtualLibraryService::new(
                pool,
                server_storage,
                media_storage,
            )),
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

    #[tokio::test]
    async fn invalid_token_cannot_adopt_user_from_request_body() {
        let state = create_test_app_state().await;
        let victim = state
            .user_authorization
            .get_or_create_user("victim", &"password123".into())
            .await
            .unwrap();
        let uri: http::Uri = "/Sessions/Capabilities/Full".parse().unwrap();
        let mut request = Request::builder()
            .method(http::Method::POST)
            .uri(uri.clone())
            .header(http::header::CONTENT_TYPE, "application/json")
            .header("X-MediaBrowser-Token", "invalid-token")
            .body(axum::body::Body::from(
                serde_json::json!({ "UserId": victim.id }).to_string(),
            ))
            .unwrap();
        request.extensions_mut().insert(OriginalUri(uri));

        let (_request, auth, user, sessions, _analysis) =
            extract_request_infos(request, &state, None).await.unwrap();

        assert!(auth.is_some());
        assert!(user.is_none());
        assert!(sessions.is_none());
    }

    #[tokio::test]
    async fn playback_reports_route_without_cached_session_authority() {
        let state = create_test_app_state().await;
        let caller = state
            .user_authorization
            .get_or_create_user("caller", &"password123".into())
            .await
            .unwrap();
        let server_id = state
            .server_storage
            .add_server(
                "Playback",
                "http://playback.example",
                100,
                crate::config::MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = state
            .server_storage
            .get_server_by_id(server_id)
            .await
            .unwrap()
            .unwrap();
        let item = state
            .media_storage
            .get_or_create_media_mapping("item", &server)
            .await
            .unwrap();
        let source = state
            .media_storage
            .get_or_create_media_mapping("source", &server)
            .await
            .unwrap();
        let now = chrono::Utc::now();
        let sessions = Some(vec![(
            AuthorizationSession {
                id: 1,
                user_id: caller.id.clone(),
                mapping_id: 1,
                server_url: server.url.to_string(),
                device: Device::from_useragent("Test"),
                jellyfin_token: "upstream-token".into(),
                original_user_id: "upstream-user".into(),
                expires_at: None,
                created_at: now,
                updated_at: now,
            },
            server.clone(),
        )]);

        for (path, action) in [
            ("/Sessions/Playing", PlaybackSessionAction::Start),
            ("/Sessions/Playing/Progress", PlaybackSessionAction::Refresh),
            ("/Sessions/Playing/Stopped", PlaybackSessionAction::Remove),
        ] {
            for session_id in [
                None,
                Some(serde_json::Value::Null),
                Some(serde_json::json!("unknown-session")),
            ] {
                let mut body = serde_json::json!({
                    "ItemId": item.virtual_media_id,
                    "MediaSourceId": source.virtual_media_id,
                    "NowPlayingQueue": [{"Id": "unrelated-queue-item"}]
                });
                if let Some(session_id) = session_id {
                    body["PlaySessionId"] = session_id;
                }
                let uri: http::Uri = path.parse().unwrap();
                let mut request = Request::builder()
                    .method(http::Method::POST)
                    .uri(uri.clone())
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .header("X-MediaBrowser-Token", &caller.virtual_key)
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap();
                request.extensions_mut().insert(OriginalUri(uri));
                let (_, _, user, _, analysis) =
                    extract_request_infos(request, &state, Some(action))
                        .await
                        .unwrap();
                assert_eq!(user.unwrap().id, caller.id);
                let mut analysis = analysis.unwrap();
                assert!(analysis.authoritative_play_session.is_none());
                let (selected, auth, matched) =
                    resolve_playback_report_server(&sessions, &analysis, &state)
                        .await
                        .unwrap();
                assert_eq!(selected.id, server.id);
                assert_eq!(auth.unwrap().user_id, caller.id);
                assert!(matched);
                analysis.requested_play_source_id = None;
                assert_eq!(
                    resolve_playback_report_server(&sessions, &analysis, &state)
                        .await
                        .unwrap()
                        .0
                        .id,
                    server.id
                );
                assert!(resolve_playback_report_server(&None, &analysis, &state)
                    .await
                    .is_err());
                analysis.authoritative_play_session = Some(PlaybackSession {
                    session_id: "known-session".into(),
                    item_id: item.virtual_media_id.clone(),
                    user_id: caller.id.clone(),
                    server_id: ServerId::new(server.id.as_i64() + 1),
                });
                assert!(resolve_playback_report_server(&sessions, &analysis, &state)
                    .await
                    .is_err());
                analysis.authoritative_play_session = None;
                analysis.requested_play_source_id = Some("unknown-source".into());
                assert!(resolve_playback_report_server(&sessions, &analysis, &state)
                    .await
                    .is_err());
                analysis.requested_play_source_id = None;
                analysis.requested_play_item_id = Some("unknown-item".into());
                assert!(resolve_playback_report_server(&sessions, &analysis, &state)
                    .await
                    .is_err());
            }
        }
    }

    #[tokio::test]
    async fn playback_report_rejects_ambiguous_aggregate_without_session_or_source() {
        use crate::media_identity::{MediaAlias, MediaKind, MediaObservation, MediaProvider};
        use crate::media_storage_service::{MediaCatalogSnapshot, MediaVersionSourceObservation};
        use std::collections::BTreeSet;

        let state = create_test_app_state().await;
        let mut snapshots = Vec::new();
        let mut sessions = Vec::new();
        let mut mappings = Vec::new();
        for name in ["first", "second"] {
            let id = state
                .server_storage
                .add_server(
                    name,
                    &format!("http://{name}.example"),
                    100,
                    crate::config::MediaStreamingMode::Redirect,
                )
                .await
                .unwrap();
            let server = state
                .server_storage
                .get_server_by_id(id)
                .await
                .unwrap()
                .unwrap();
            let mapping = state
                .media_storage
                .get_or_create_media_mapping("item", &server)
                .await
                .unwrap();
            snapshots.push(MediaCatalogSnapshot {
                source_key: name.into(),
                server_id: id,
                complete: true,
                observations: vec![MediaObservation {
                    virtual_media_id: mapping.virtual_media_id.clone(),
                    aliases: BTreeSet::from([MediaAlias {
                        provider: MediaProvider::Tmdb,
                        kind: MediaKind::Movie,
                        provider_id: "42".into(),
                    }]),
                }],
            });
            mappings.push(mapping);
            let now = chrono::Utc::now();
            sessions.push((
                AuthorizationSession {
                    id: id.as_i64(),
                    user_id: "caller".into(),
                    mapping_id: id.as_i64(),
                    server_url: server.url.to_string(),
                    device: Device::from_useragent("Test"),
                    jellyfin_token: "upstream-token".into(),
                    original_user_id: "upstream-user".into(),
                    expires_at: None,
                    created_at: now,
                    updated_at: now,
                },
                server,
            ));
        }
        let generation = state
            .media_storage
            .begin_media_reconciliation()
            .await
            .unwrap();
        let groups = state
            .media_storage
            .reconcile_media_catalog("configured:library:caller", generation, &snapshots, true)
            .await
            .unwrap();
        let aggregate_id = groups[&mappings[0].virtual_media_id]
            .virtual_media_id
            .clone();
        let group = state
            .media_storage
            .get_media_version_group(&aggregate_id)
            .await
            .unwrap()
            .unwrap();
        let source = state
            .media_storage
            .get_or_create_media_mapping("source", &sessions[1].1)
            .await
            .unwrap();
        state
            .media_storage
            .replace_media_version_sources(
                group.id,
                generation,
                &[mappings[1].id],
                &[MediaVersionSourceObservation {
                    member_mapping_id: mappings[1].id,
                    source_virtual_id: source.virtual_media_id.clone(),
                }],
            )
            .await
            .unwrap();
        let sessions = Some(sessions);
        let mut analysis = RequestBodyAnalysisResult {
            requested_play_item_id: Some(aggregate_id),
            ..Default::default()
        };
        assert!(resolve_playback_report_server(&sessions, &analysis, &state)
            .await
            .is_err());
        analysis.requested_play_source_id = Some(source.virtual_media_id.clone());
        assert_eq!(
            resolve_playback_report_server(&sessions, &analysis, &state)
                .await
                .unwrap()
                .0
                .id,
            mappings[1].server_id
        );
        analysis.requested_play_item_id = Some(mappings[0].virtual_media_id.clone());
        assert!(resolve_playback_report_server(&sessions, &analysis, &state)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn playback_reports_reject_missing_auth_and_conflicting_fields() {
        let state = create_test_app_state().await;
        let caller = state
            .user_authorization
            .get_or_create_user("caller", &"password123".into())
            .await
            .unwrap();
        for (token, body) in [
            (
                "invalid",
                serde_json::json!({"ItemId": "item", "UserId": caller.id}),
            ),
            (
                caller.virtual_key.as_str(),
                serde_json::json!({"ItemId": "item", "itemId": "different"}),
            ),
            (
                caller.virtual_key.as_str(),
                serde_json::json!({"ItemId": "item", "PlaySessionId": "one", "playSessionId": "two"}),
            ),
            (
                caller.virtual_key.as_str(),
                serde_json::json!({"ItemId": "item", "MediaSourceId": "one", "mediaSourceId": "two"}),
            ),
            (
                caller.virtual_key.as_str(),
                serde_json::json!({"NowPlayingQueue": [{"ItemId": "item"}]}),
            ),
        ] {
            let uri: http::Uri = "/Sessions/Playing/Progress".parse().unwrap();
            let mut request = Request::builder()
                .method(http::Method::POST)
                .uri(uri.clone())
                .header(http::header::CONTENT_TYPE, "application/json")
                .header("X-MediaBrowser-Token", token)
                .body(axum::body::Body::from(body.to_string()))
                .unwrap();
            request.extensions_mut().insert(OriginalUri(uri));
            assert!(
                extract_request_infos(request, &state, Some(PlaybackSessionAction::Refresh))
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn progress_rejects_a_session_owned_by_another_user() {
        let state = create_test_app_state().await;
        let caller = state
            .user_authorization
            .get_or_create_user("caller", &"password123".into())
            .await
            .unwrap();
        state
            .play_sessions
            .add_session(PlaybackSession {
                session_id: "victim-session".to_string(),
                item_id: "item".to_string(),
                user_id: "victim".to_string(),
                server_id: ServerId::new(1),
            })
            .await;
        let uri: http::Uri = "/Sessions/Playing/Progress".parse().unwrap();
        let mut request = Request::builder()
            .method(http::Method::POST)
            .uri(uri.clone())
            .header(http::header::CONTENT_TYPE, "application/json")
            .header("X-MediaBrowser-Token", caller.virtual_key)
            .body(axum::body::Body::from(
                serde_json::json!({
                    "PlaySessionId": "victim-session",
                    "ItemId": "item"
                })
                .to_string(),
            ))
            .unwrap();
        request.extensions_mut().insert(OriginalUri(uri));

        let result =
            extract_request_infos(request, &state, Some(PlaybackSessionAction::Refresh)).await;

        assert!(result.is_err());
    }
}
