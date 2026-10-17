//! Recognize playback reports and retain test helpers for backend-specific playback routing.
use super::*;
pub(crate) async fn playback_session_action(
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

#[cfg(test)]
pub async fn apply_to_request(
    request: &mut reqwest::Request,
    server: &Server,
    session: &Option<AuthorizationSession>,
    auth: &Option<JellyfinAuthorization>,
    state: &AppState,
    access_scope: Option<&VirtualLibraryAccessScope>,
) {
    let prefix = state.get_url_prefix().await;
    crate::upstream::jellyfin::request::UpstreamRequestBuilder::new(
        &state.processors.url_processor,
        prefix.as_deref(),
    )
    .apply(request, server, session, auth, access_scope)
    .await;
}

// Reports must not use the generic best-server fallback: a wrong destination can
// update another copy's playback state. Only top-level item/source fields route them.
#[cfg(test)]
pub(super) async fn resolve_playback_report_server(
    sessions: &Option<Vec<(AuthorizationSession, Server)>>,
    analysis: &RequestBodyAnalysisResult,
    state: &AppState,
) -> Result<(Server, Option<AuthorizationSession>, bool)> {
    crate::application::playback::reports::resolve_server(sessions, analysis, &state.media_storage)
        .await
        .map(|target| (target.server, Some(target.session), true))
}

pub(crate) use crate::application::playback::reports::NoAvailableBackend;

pub(crate) use crate::application::error::MediaAccessDenied;
