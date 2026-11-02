//! Collect the caller context for one client request: resolve the proxy user,
//! analyze the JSON body and load the authorized, healthy backend sessions.
//! Backend *selection* happens later, in `routing`.
use anyhow::{anyhow, Result};
use tracing::{debug, error};

use crate::application::accounts::user_for_credential;
use crate::application::{
    media_services::MediaServices,
    request::{context::RequestContext, input::OperationRequest},
};
use crate::auth::service::{AuthorizationSession, LocalUser};
use crate::libraries::service::VirtualLibraryAccessScope;
use crate::protocol::caller::{request_device, JellyfinAuthorization};
use crate::protocol::device::Device;
use crate::protocol::routes::PlaybackSessionAction;
use crate::servers::storage::Server;
use crate::translation::analyze_json;
use crate::translation::request_analyzer::{RequestAnalysisContext, RequestBodyAnalysisResult};

#[cfg(test)]
mod tests;

/// `request` is the buffered client request (see `http::incoming`), still
/// addressed to the proxy with virtual IDs and proxy credentials.
pub(crate) async fn operation(
    request: reqwest::Request,
    state: &MediaServices,
    playback_session_action: Option<PlaybackSessionAction>,
) -> Result<OperationRequest> {
    // 1. Who is calling?
    let auth = JellyfinAuthorization::from_headers_url(request.headers(), request.url());
    debug!(authorization = ?auth, "Extracted authorization");
    let device = request_device(request.headers(), request.url(), auth.as_ref());
    let mut user = user_for_credential(auth.as_ref(), &state.user_authorization).await?;
    if let (Some(user), Some(token)) = (user.as_ref(), auth.as_ref().and_then(|a| a.token_ref())) {
        state
            .client_sessions
            .ensure(user, token, device.as_ref())
            .await;
    }

    // 2. Which IDs, users and play sessions does the JSON body reference?
    let body_analysis = analyze_body(
        &request,
        state,
        user.as_ref().map(|user| user.id.clone()),
        playback_session_action,
    )
    .await?;
    if let Some(found_user) = body_analysis.as_ref().and_then(|a| a.get_user()) {
        debug!(user_id = %found_user.id, "Found user in request body");
        // Legacy metadata only: an unauthenticated body never authenticates.
        if user.is_none() && auth.is_none() {
            user = Some(found_user);
        }
    }

    // 3. Which backends may this caller use, and which are usable right now?
    let (sessions, access_scope) = match (&auth, &user) {
        (Some(auth), Some(user)) => {
            let (sessions, scope) =
                backend_sessions(state, &request, auth, user, device.as_ref()).await?;
            (
                Some(sessions).filter(|sessions| !sessions.is_empty()),
                Some(scope),
            )
        }
        (_, user) => (
            None,
            user.as_ref()
                .map(|user| VirtualLibraryAccessScope::new(&user.id, [])),
        ),
    };

    let context = RequestContext::new(&request, auth, user, sessions, access_scope);
    Ok(OperationRequest {
        original_request: request,
        context,
        body_analysis,
    })
}

async fn analyze_body(
    request: &reqwest::Request,
    state: &MediaServices,
    authenticated_user_id: Option<crate::domain::user_id::UserId>,
    playback_session_action: Option<PlaybackSessionAction>,
) -> Result<Option<RequestBodyAnalysisResult>> {
    let Some(json) = body_to_json(request) else {
        if playback_session_action.is_some() {
            return Err(anyhow!("playback session report requires a JSON body"));
        }
        return Ok(None);
    };
    let is_authenticated = authenticated_user_id.is_some();
    let context = RequestAnalysisContext {
        authenticated_user_id,
        playback_session_action,
    };
    let analysis = analyze_json(
        &json,
        &state.translation.request_analyzer,
        &context,
        RequestBodyAnalysisResult::default(),
    )
    .await?;
    if playback_session_action.is_some()
        && (analysis.requested_play_item_id.is_none() || !is_authenticated)
    {
        return Err(anyhow!(
            "playback report requires an authenticated user and item"
        ));
    }
    if let Some(found_server) = analysis.get_server() {
        debug!("Found server in request body: {}", &found_server.name);
    }
    Ok(Some(analysis))
}

/// Return the caller's healthy backend sessions for this device and the full
/// mapped access scope. Authorization and availability are distinct: mapped
/// servers stay in scope even when offline or missing a session for this device.
async fn backend_sessions(
    state: &MediaServices,
    request: &reqwest::Request,
    auth: &JellyfinAuthorization,
    user: &LocalUser,
    device: Option<&Device>,
) -> Result<(
    Vec<(AuthorizationSession, Server)>,
    VirtualLibraryAccessScope,
)> {
    // Query-only DeviceId identifies a local player, but does not include
    // the client metadata required by upstream Device::matches. Preserve
    // token-only media delivery's existing lookup across this user's sessions.
    let upstream_device = auth.get_device(request.headers());
    let partial = matches!(auth,
        JellyfinAuthorization::Authorization(a) | JellyfinAuthorization::XEmbyAuthorization(a)
        if !a.has_complete_metadata());
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
        if let Some(device) = device {
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

    let mapped_servers = state
        .user_authorization
        .get_mapped_servers(&user.id)
        .await?;
    let access_scope =
        VirtualLibraryAccessScope::new(&user.id, mapped_servers.iter().map(|server| server.id));

    let mut healthy = Vec::with_capacity(sessions.len());
    for (session, server) in sessions {
        if state
            .server_storage
            .server_status(server.id)
            .await
            .is_healthy()
        {
            healthy.push((session, server));
        }
    }
    Ok((healthy, access_scope))
}

/// Parse a non-empty `application/json` body; anything else is opaque.
fn body_to_json(request: &reqwest::Request) -> Option<serde_json::Value> {
    let content_type = request
        .headers()
        .get(reqwest::header::CONTENT_TYPE)?
        .to_str()
        .ok()?;
    if !content_type.contains("application/json") {
        return None;
    }
    let bytes = request.body()?.as_bytes().unwrap_or_default();
    if bytes.is_empty() {
        return None;
    }
    serde_json::from_slice(bytes)
        .inspect_err(|error| error!("Failed to parse JSON body: {error}"))
        .ok()
}
