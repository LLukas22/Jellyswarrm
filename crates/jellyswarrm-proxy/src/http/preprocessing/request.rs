//! Convert and consume incoming HTTP requests, then analyze bodies and resolve authorized backend sessions.
use super::*;
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
    Option<VirtualLibraryAccessScope>,
)> {
    let request = axum_to_reqwest(req).await?;

    let auth = JellyfinAuthorization::from_request(&request);

    if let Some(auth) = &auth {
        debug!("Extracted authorization: {:?}", auth);
    } else {
        debug!("No authorization found in request");
    }

    let device = request_device(&request, auth.as_ref());
    let mut user = get_user_from_request(&auth, state).await?;
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
        let mut sessions = state
            .user_authorization
            .get_user_sessions(&user.id, upstream_device)
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

    Ok((
        request,
        auth,
        user,
        sessions,
        request_body_result,
        access_scope,
    ))
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
