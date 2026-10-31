//! Incoming request extraction: convert Axum requests, resolve the caller identity,
//! analyze bodies, and collect authorized backend sessions. Workflows own backend selection.
use axum::extract::{OriginalUri, Request};

use anyhow::{anyhow, Result};
use axum::http;
use http_body_util::BodyExt;
use tracing::{debug, error};

use crate::application::accounts::{self, user_for_credential, RequestIdentity};
use crate::application::{
    request::{context::RequestContext, input::OperationRequest},
    services::MediaServices,
};
use crate::auth::service::AuthorizationSession;
use crate::libraries::service::VirtualLibraryAccessScope;
use crate::processors::analyze_json;
use crate::processors::request_analyzer::{
    PlaybackSessionAction, RequestAnalysisContext, RequestBodyAnalysisResult,
};
use crate::protocol::identity::{request_device, JellyfinAuthorization};
use crate::servers::storage::Server;

#[cfg(test)]
#[path = "../unit_tests/preprocessing.rs"]
mod tests;

/// Credential parsing reads the query, so give the request URI an absolute form.
pub(crate) fn request_url(uri: &http::Uri) -> Result<url::Url> {
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    Ok(url::Url::parse(&format!(
        "http://localhost{path_and_query}"
    ))?)
}

pub(crate) async fn resolve_request_identity(
    headers: &http::HeaderMap,
    uri: &http::Uri,
    state: &crate::auth::UserAuthorizationService,
) -> Result<RequestIdentity> {
    Ok(accounts::resolve_identity(headers, &request_url(uri)?, state).await?)
}

pub(crate) async fn extract_request_infos(
    req: Request,
    state: &MediaServices,
    playback_session_action: Option<PlaybackSessionAction>,
) -> Result<OperationRequest> {
    let request = axum_to_reqwest(req).await?;

    let auth = JellyfinAuthorization::from_headers_url(request.headers(), request.url());

    if let Some(auth) = &auth {
        debug!("Extracted authorization: {:?}", auth);
    } else {
        debug!("No authorization found in request");
    }

    let device = request_device(request.headers(), request.url(), auth.as_ref());
    let mut user = user_for_credential(auth.as_ref(), &state.user_authorization).await?;
    if let (Some(user), Some(token)) = (user.as_ref(), auth.as_ref().and_then(|a| a.token_ref())) {
        state
            .client_sessions
            .ensure(user, token, device.as_ref())
            .await;
    }
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
            debug!(user_id = %found_user.id, "Found user in request body");
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

    let mut access_scope = user
        .as_ref()
        .map(|user| VirtualLibraryAccessScope::new(&user.id, []));
    let sessions = if auth.is_none() {
        None
    } else if let Some(user) = &user {
        // Query-only DeviceId identifies a local player, but does not include
        // the client metadata required by upstream Device::matches. Preserve
        // token-only media delivery's existing lookup across this user's sessions.
        let upstream_device = auth
            .as_ref()
            .and_then(|auth| auth.get_device(request.headers()));
        let partial = matches!(auth.as_ref(), Some(
            JellyfinAuthorization::Authorization(a) | JellyfinAuthorization::XEmbyAuthorization(a)
        ) if !a.has_complete_metadata());
        let mut sessions = if partial {
            state
                .user_authorization
                .get_user_sessions_for_partial_device(&user.id, upstream_device.as_ref())
                .await?
        } else {
            state
                .user_authorization
                .get_user_sessions(&user.id, upstream_device)
                .await?
        };

        // ANDROID TV DEVICE-ID REBIND (intentional behavior):
        // Android TV can authenticate with a username-derived device ID and then switch to a
        // user-id-derived device ID on the very next authenticated request. Our normal session
        // lookup is strict on device ID, so the first request after login may not find a match.
        // To keep the rest of the pipeline unchanged, we do a one-time Android-TV-only rebind
        // when strict lookup returns no session, then re-run strict lookup.
        if sessions.is_empty() && !partial {
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

        // Authorization and availability are distinct: mapped sources remain
        // in scope even when offline or missing a session for this device.
        let mapped_servers = state
            .user_authorization
            .get_mapped_servers(&user.id)
            .await?;
        access_scope = Some(VirtualLibraryAccessScope::new(
            &user.id,
            mapped_servers.iter().map(|server| server.id),
        ));
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

    let context = RequestContext::new(&request, auth, user, sessions, access_scope);
    Ok(OperationRequest {
        original_request: request,
        context,
        body_analysis: request_body_result,
    })
}

async fn axum_to_reqwest(req: Request) -> Result<reqwest::Request> {
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
fn body_to_json(request: &reqwest::Request) -> Option<serde_json::Value> {
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
