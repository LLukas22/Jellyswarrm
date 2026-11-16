//! Copy watch state from the copy that was played to every other copy of a
//! merged item, so backends agree without an external sync tool.
//!
//! A playback report reaches only the backend that is streaming. Once that
//! backend has applied its own rules (played threshold, resume position reset)
//! the proxy reads the result back and writes the position, played flag and
//! last-played date to the viewer's other authorized copies. Writes run in the
//! background: a failed copy is logged and never fails the client's report.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde_json::{Map, Value};
use tracing::{debug, warn};

use crate::{
    application::{
        catalog::snapshots::CatalogSnapshots, media_services::MediaServices,
        request::context::AuthorizedTarget, request::input::OperationRequest,
    },
    media::storage::MediaMapping,
    protocol::routes::PlaybackSessionAction,
};

/// Requests prepared while the client's report is in hand, run afterwards.
pub(crate) struct WatchStateSync {
    viewer: String,
    key: SyncKey,
    generation: u64,
    /// `GET /UserItems/{id}/UserData` on the backend that was played.
    read: reqwest::Request,
    /// `POST /UserItems/{id}/UserData` on each other copy, body set on run.
    writes: Vec<(reqwest::Request, String)>,
}

/// Plan a sync after the backend accepted a playback report. Returns `None`
/// when syncing is off, throttled, or the item has no other copies. Problems
/// are logged rather than returned: they must not affect the report itself.
pub(crate) async fn plan(
    state: &MediaServices,
    input: &OperationRequest,
    path: &str,
    action: PlaybackSessionAction,
    member: &MediaMapping,
    owner: &AuthorizedTarget,
) -> Option<WatchStateSync> {
    let (enabled, progress_interval) = {
        let config = state.config.read().await;
        (
            config.sync_watch_state && config.deduplicate_media,
            Duration::from_secs(config.watch_state_progress_sync_secs),
        )
    };
    if !enabled {
        return None;
    }
    let scope = input.context.access_scope()?;
    let viewer = scope.user_id().to_string();
    let key = SyncKey {
        viewer: viewer.clone(),
        member: member.id,
    };
    let generation = admit(&key, action, progress_interval, Instant::now())?;

    let copies = match state
        .media_storage
        .get_media_version_copies(member.id, &viewer)
        .await
    {
        Ok(copies) => copies,
        Err(error) => {
            warn!("Watch state sync skipped: could not load copies: {error}");
            return None;
        }
    };
    let prefix = proxy_prefix(input.original_request.url().path(), path);
    let mut writes = Vec::new();
    for copy in copies {
        if copy.mapping.id == member.id || !scope.allows(copy.server.id) {
            continue;
        }
        let Ok(target) = input.context.target(copy.server.id) else {
            debug!(
                "Watch state sync skips {}: no session for this viewer",
                copy.server.name
            );
            continue;
        };
        let request = user_data_request(
            &input.original_request,
            prefix,
            copy.mapping.virtual_media_id.as_str(),
            reqwest::Method::POST,
        )?;
        match state.request_builder().build(&target, &request).await {
            Ok(request) => writes.push((request, copy.server.name.clone())),
            Err(error) => warn!("Watch state sync skips {}: {:?}", copy.server.name, error.0),
        }
    }
    if writes.is_empty() {
        return None;
    }
    let read = user_data_request(
        &input.original_request,
        prefix,
        member.virtual_media_id.as_str(),
        reqwest::Method::GET,
    )?;
    let read = match state.request_builder().build(owner, &read).await {
        Ok(read) => read,
        Err(error) => {
            warn!("Watch state sync skipped: {:?}", error.0);
            return None;
        }
    };
    Some(WatchStateSync {
        viewer,
        key,
        generation,
        read,
        writes,
    })
}

impl WatchStateSync {
    pub(crate) async fn run(self, client: reqwest::Client, snapshots: Arc<CatalogSnapshots>) {
        let data: Value =
            match crate::application::request::response::execute_json_request(&client, self.read)
                .await
            {
                Ok(data) => data,
                Err(error) => {
                    warn!("Watch state sync skipped: could not read played copy: {error}");
                    return;
                }
            };
        let Some(update) = user_data_update(&data) else {
            debug!("Watch state sync skipped: played copy reported no watch state");
            return;
        };
        let body = match serde_json::to_vec(&update) {
            Ok(body) => body,
            Err(error) => {
                warn!("Watch state sync skipped: {error}");
                return;
            }
        };
        for (mut request, server) in self.writes {
            // A later report for this item supersedes this one; never let a
            // slow progress sync overwrite the state written on stop.
            if !is_current(&self.key, self.generation) {
                debug!("Watch state sync superseded by a newer report");
                break;
            }
            request.headers_mut().insert(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_static("application/json"),
            );
            request
                .headers_mut()
                .remove(reqwest::header::CONTENT_LENGTH);
            *request.body_mut() = Some(body.clone().into());
            match client.execute(request).await {
                Ok(response) if response.status().is_success() => {
                    debug!("Synced watch state to {server}")
                }
                Ok(response) => warn!(
                    "Watch state sync to {server} failed: HTTP {}",
                    response.status()
                ),
                Err(error) => warn!(
                    "Watch state sync to {server} failed: {}",
                    error.without_url()
                ),
            }
        }
        snapshots.invalidate_viewer(&self.viewer);
    }
}

/// The part of a backend's `UserItemDataDto` that describes watch progress, as
/// an `UpdateUserItemDataDto`. Favorites, ratings and play counts stay local.
fn user_data_update(data: &Value) -> Option<Value> {
    let data = data.as_object()?;
    let field = |name: &str| {
        data.iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
            .filter(|value| !value.is_null())
    };
    let mut update = Map::new();
    if let Some(played) = field("Played").filter(|value| value.is_boolean()) {
        update.insert("Played".into(), played.clone());
    }
    if let Some(position) = field("PlaybackPositionTicks").filter(|value| value.is_i64()) {
        update.insert("PlaybackPositionTicks".into(), position.clone());
    }
    if update.is_empty() {
        return None;
    }
    if let Some(date) = field("LastPlayedDate").filter(|value| value.is_string()) {
        update.insert("LastPlayedDate".into(), date.clone());
    }
    Some(Value::Object(update))
}

fn user_data_request(
    original: &reqwest::Request,
    prefix: &str,
    item: &str,
    method: reqwest::Method,
) -> Option<reqwest::Request> {
    let mut url = original.url().clone();
    url.set_path(&format!("{prefix}/UserItems/{item}/UserData"));
    url.set_query(None);
    url.set_fragment(None);
    let mut request = reqwest::Request::new(method, url);
    for (name, value) in original.headers() {
        if name != reqwest::header::CONTENT_LENGTH && name != reqwest::header::CONTENT_TYPE {
            request.headers_mut().append(name, value.clone());
        }
    }
    // The report's own query may carry an api_key; keep only that.
    let api_key = original
        .url()
        .query_pairs()
        .filter(|(key, _)| {
            key.eq_ignore_ascii_case("api_key") || key.eq_ignore_ascii_case("ApiKey")
        })
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    if !api_key.is_empty() {
        request.url_mut().query_pairs_mut().extend_pairs(api_key);
    }
    Some(request)
}

/// `full_path` is the client's path, `path` the same path without the proxy
/// prefix; the difference is the prefix to keep on synthesized requests.
fn proxy_prefix<'a>(full_path: &'a str, path: &str) -> &'a str {
    full_path
        .len()
        .checked_sub(path.len())
        .and_then(|end| full_path.get(..end))
        .map(|prefix| prefix.trim_end_matches('/'))
        .unwrap_or("")
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SyncKey {
    viewer: String,
    /// Mapping ID of the copy that was played.
    member: i64,
}

#[derive(Debug, Clone, Copy)]
struct SyncSlot {
    generation: u64,
    synced_at: Instant,
}

const FORGET_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

fn slots() -> &'static Mutex<HashMap<SyncKey, SyncSlot>> {
    static SLOTS: OnceLock<Mutex<HashMap<SyncKey, SyncSlot>>> = OnceLock::new();
    SLOTS.get_or_init(Default::default)
}

/// Decide whether a report triggers a sync and, if so, claim the next
/// generation for its item. Stops always sync; progress at most once per
/// `progress_interval` (never when it is zero); starts never.
fn admit(
    key: &SyncKey,
    action: PlaybackSessionAction,
    progress_interval: Duration,
    now: Instant,
) -> Option<u64> {
    let mut slots = slots().lock().expect("watch sync slots poisoned");
    slots.retain(|_, slot| now.saturating_duration_since(slot.synced_at) < FORGET_AFTER);
    let previous = slots.get(key).copied();
    match action {
        PlaybackSessionAction::Start => return None,
        PlaybackSessionAction::Refresh => {
            if progress_interval.is_zero()
                || previous.is_some_and(|slot| {
                    now.saturating_duration_since(slot.synced_at) < progress_interval
                })
            {
                return None;
            }
        }
        PlaybackSessionAction::Remove => {}
    }
    let generation = previous.map_or(1, |slot| slot.generation + 1);
    slots.insert(
        key.clone(),
        SyncSlot {
            generation,
            synced_at: now,
        },
    );
    Some(generation)
}

fn is_current(key: &SyncKey, generation: u64) -> bool {
    slots()
        .lock()
        .expect("watch sync slots poisoned")
        .get(key)
        .is_none_or(|slot| slot.generation == generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(member: i64) -> SyncKey {
        SyncKey {
            viewer: format!("watch-sync-test-{}", uuid::Uuid::new_v4()),
            member,
        }
    }

    #[test]
    fn copies_progress_fields_only() {
        let update = user_data_update(&json!({
            "PlaybackPositionTicks": 0, "Played": true, "PlayCount": 3,
            "IsFavorite": true, "Rating": 9.0, "Key": "k", "ItemId": "backend",
            "LastPlayedDate": "2026-11-11T18:00:00.0000000Z", "PlayedPercentage": 100.0
        }))
        .unwrap();
        assert_eq!(
            update,
            json!({
                "Played": true, "PlaybackPositionTicks": 0,
                "LastPlayedDate": "2026-11-11T18:00:00.0000000Z"
            })
        );
        let update = user_data_update(&json!({
            "playbackPositionTicks": 123, "played": false, "lastPlayedDate": null
        }))
        .unwrap();
        assert_eq!(
            update,
            json!({"Played": false, "PlaybackPositionTicks": 123})
        );
        assert!(user_data_update(&json!({"IsFavorite": true})).is_none());
        assert!(user_data_update(&json!([])).is_none());
    }

    #[test]
    fn stops_always_sync_progress_is_throttled_and_starts_never_sync() {
        let key = key(1);
        let minute = Duration::from_secs(60);
        let now = Instant::now();
        assert_eq!(admit(&key, PlaybackSessionAction::Start, minute, now), None);
        assert_eq!(
            admit(&key, PlaybackSessionAction::Refresh, minute, now),
            Some(1)
        );
        let soon = now + Duration::from_secs(10);
        assert_eq!(
            admit(&key, PlaybackSessionAction::Refresh, minute, soon),
            None
        );
        assert_eq!(
            admit(&key, PlaybackSessionAction::Remove, minute, soon),
            Some(2)
        );
        let later = soon + minute;
        assert_eq!(
            admit(&key, PlaybackSessionAction::Refresh, minute, later),
            Some(3)
        );
        let other = self::key(1);
        let zero = Duration::ZERO;
        assert_eq!(
            admit(&other, PlaybackSessionAction::Refresh, zero, now),
            None
        );
        assert_eq!(
            admit(&other, PlaybackSessionAction::Remove, zero, now),
            Some(1)
        );
    }

    #[test]
    fn a_newer_report_supersedes_an_older_sync() {
        let key = key(7);
        let now = Instant::now();
        let first = admit(
            &key,
            PlaybackSessionAction::Refresh,
            Duration::from_secs(60),
            now,
        )
        .unwrap();
        assert!(is_current(&key, first));
        let stop = admit(
            &key,
            PlaybackSessionAction::Remove,
            Duration::from_secs(60),
            now,
        )
        .unwrap();
        assert!(!is_current(&key, first));
        assert!(is_current(&key, stop));
    }

    #[test]
    fn synthesized_requests_keep_the_proxy_prefix() {
        assert_eq!(
            proxy_prefix(
                "/jellyfin/Sessions/Playing/Stopped",
                "/Sessions/Playing/Stopped"
            ),
            "/jellyfin"
        );
        assert_eq!(
            proxy_prefix("/Sessions/Playing/Stopped", "/Sessions/Playing/Stopped"),
            ""
        );
        let mut original = reqwest::Request::new(
            reqwest::Method::POST,
            "http://proxy/jellyfin/Sessions/Playing/Stopped?api_key=secret&Other=1"
                .parse()
                .unwrap(),
        );
        original.headers_mut().insert(
            reqwest::header::CONTENT_LENGTH,
            reqwest::header::HeaderValue::from_static("42"),
        );
        original.headers_mut().insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static("MediaBrowser Token=\"t\""),
        );
        let request =
            user_data_request(&original, "/jellyfin", "item", reqwest::Method::POST).unwrap();
        assert_eq!(
            request.url().as_str(),
            "http://proxy/jellyfin/UserItems/item/UserData?api_key=secret"
        );
        assert!(request
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .is_none());
        assert!(request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .is_some());
    }
}
