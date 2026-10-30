//! Shared response processing: project upstream media into client identities and
//! record returned media for client-session observation. Callers choose when to
//! observe; projection alone never publishes a playback binding.

pub(crate) async fn media_response(
    sessions: &crate::sessions::ClientSessionService,
    token: Option<&str>,
    payload: &serde_json::Value,
) {
    if let Some(token) = token {
        sessions.cache_media_response(token, payload).await;
    }
}
