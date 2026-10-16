//! Named client-observation hooks, outside catalog cache and wire transport.
use serde_json::Value;

use crate::AppState;

/// Preserve the existing upstream-success observation point. This is not a
/// playback-binding commit: those transitions remain revision-fenced and are
/// committed only after their operation's required response validation.
pub(crate) async fn forwarded_success(
    state: &AppState,
    original: &reqwest::Request,
    mutation_viewer: Option<&str>,
) {
    if let Some(viewer) = mutation_viewer {
        state.catalog_snapshots.invalidate_viewer(viewer);
    }
    crate::sessions::observe_playback(state, original).await;
}

pub(crate) async fn media_response(state: &AppState, token: Option<&str>, payload: &Value) {
    if let Some(token) = token {
        state
            .client_sessions
            .cache_media_response(token, payload)
            .await;
    }
}
