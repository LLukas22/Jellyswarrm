//! Collect the caller context for one client request: resolve the proxy user,
//! analyze the JSON body and load the authorized, healthy backend sessions.
//! Backend *selection* happens later, in `routing`.
use anyhow::{anyhow, Result};
use tracing::{debug, error};

use crate::application::accounts::{resolve_identity, RequestIdentity};
use crate::application::{
    media_services::MediaServices,
    request::{context::RequestContext, input::OperationRequest},
};
use crate::auth::service::{AuthorizationSession, CallerCredential, LocalUser};
use crate::libraries::service::VirtualLibraryAccessScope;

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
    let RequestIdentity {
        auth,
        mut user,
        credential,
        device,
    } = resolve_identity(request.headers(), request.url(), &state.user_authorization).await?;
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
    let (sessions, access_scope) = match (&auth, &user, credential) {
        (Some(_), Some(user), Some(credential)) => {
            let (sessions, scope) = backend_sessions(state, user, credential).await?;
            (
                Some(sessions).filter(|sessions| !sessions.is_empty()),
                Some(scope),
            )
        }
        (_, user, _) => (
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

/// Return the caller's healthy backend sessions and full mapped access scope.
/// Mapped servers stay in scope while offline or awaiting a key's first login.
async fn backend_sessions(
    state: &MediaServices,
    user: &LocalUser,
    credential: CallerCredential,
) -> Result<(
    Vec<(AuthorizationSession, Server)>,
    VirtualLibraryAccessScope,
)> {
    if let CallerCredential::ApplicationKey(id) = credential {
        state
            .mapping_authenticator
            .ensure_key_sessions(user, id)
            .await?;
    }
    let sessions = state
        .user_authorization
        .sessions_for_credential(&user.id, credential)
        .await?;
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
