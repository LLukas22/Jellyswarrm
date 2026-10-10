use super::{
    library_resolution::CatalogFetchTarget, postprocessing::Pagination,
    request_policy::is_upstream_limited_catalog_request,
};
use crate::{
    request_preprocessing::{JellyfinAuthorization, PreprocessedRequest},
    AppState,
};
use axum::Json;
use hyper::StatusCode;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Bounded, token/viewer/target-scoped materialized query results. Moka coalesces
/// concurrent builds; failed or incomplete scans are never inserted.
pub(crate) struct CatalogSnapshots {
    cache: moka::future::Cache<String, Arc<serde_json::Value>>,
    revisions: Mutex<HashMap<String, u64>>,
}

impl Default for CatalogSnapshots {
    fn default() -> Self {
        Self {
            revisions: Mutex::new(HashMap::new()),
            cache: moka::future::Cache::builder()
                .max_capacity(64 * 1024 * 1024)
                .weigher(|_: &String, value: &Arc<serde_json::Value>| {
                    value.to_string().len().min(u32::MAX as usize) as u32
                })
                .time_to_live(Duration::from_secs(60))
                .build(),
        }
    }
}

impl CatalogSnapshots {
    fn revision(&self, viewer: &str) -> u64 {
        *self
            .revisions
            .lock()
            .expect("snapshot revisions poisoned")
            .get(viewer)
            .unwrap_or(&0)
    }

    /// A revision also fences off builds that were in flight during the write.
    pub(crate) fn invalidate_viewer(&self, viewer: &str) {
        let mut revisions = self.revisions.lock().expect("snapshot revisions poisoned");
        *revisions.entry(viewer.to_string()).or_default() += 1;
    }
}

pub(super) struct SnapshotResponse {
    pub response: Json<serde_json::Value>,
    pub complete: bool,
}

enum BuildFailure {
    Status(StatusCode),
    // Share a degraded response with current waiters, but never cache it.
    Degraded(Arc<serde_json::Value>),
}

#[cfg(test)]
impl CatalogSnapshots {
    pub(super) fn expire(&self) {
        self.cache.invalidate_all();
    }
}

pub(super) struct SnapshotRequest {
    key: Option<String>,
    pagination: Pagination,
    token: Option<String>,
}

impl SnapshotRequest {
    pub async fn prepare(
        state: &AppState,
        request: &mut PreprocessedRequest,
        targets: &[CatalogFetchTarget],
        scope: &str,
    ) -> Self {
        let token = request
            .auth
            .as_ref()
            .and_then(|auth| auth.token_ref())
            .map(str::to_string)
            .or_else(|| {
                JellyfinAuthorization::from_request(&request.original_request)
                    .and_then(|auth| auth.token())
            });
        let pagination = Pagination::from_url(request.original_request.url());
        let path = request
            .original_request
            .url()
            .path()
            .trim_end_matches('/')
            .to_ascii_lowercase();
        // Bounded endpoints define their own result window. Do not cache across
        // windows or pretend their response is an exhaustive catalog snapshot.
        if is_upstream_limited_catalog_request(request.original_request.url())
            || path.ends_with("/resume")
            || path.ends_with("/shows/nextup")
            || super::library_resolution::is_library_root_request(
                request.original_request.url(),
                state.get_url_prefix().await.as_deref(),
            )
        {
            return Self {
                key: None,
                pagination,
                token,
            };
        }
        let pairs = request
            .original_request
            .url()
            .query_pairs()
            .filter(|(key, _)| {
                !key.eq_ignore_ascii_case("StartIndex") && !key.eq_ignore_ascii_case("Limit")
            })
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        request
            .original_request
            .url_mut()
            .query_pairs_mut()
            .clear()
            .extend_pairs(pairs);
        let mut hash = Sha256::new();
        let viewer = request
            .access_scope
            .as_ref()
            .map(|scope| scope.user_id())
            .or_else(|| request.user.as_ref().map(|user| user.id.as_str()))
            .unwrap_or("anonymous");
        hash.update(state.catalog_snapshots.revision(viewer).to_le_bytes());
        hash.update(format!(
            "{scope}|{}|{:?}|{:?}|{:?}|{:?}",
            request.original_request.url(),
            request.original_request.headers(),
            request.access_scope,
            request.user.as_ref().map(|user| &user.id),
            token
        ));
        let config = state.config.read().await;
        hash.update(format!(
            "{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}",
            config.deduplicate_media,
            config.include_server_name_in_media,
            config.server_id,
            config.url_prefix,
            config.public_address,
            config.server_name,
            config.media_streaming_mode
        ));
        drop(config);
        for target in targets {
            hash.update(format!(
                "{:?}|{:?}|{:?}|{}|{}|{}",
                target.server,
                target.parent_id,
                target.resolved_parent_id,
                target.session.id,
                target.session.original_user_id,
                target.session.jellyfin_token
            ));
        }
        Self {
            key: Some(hex::encode(hash.finalize())),
            pagination,
            token,
        }
    }

    pub async fn serve(
        self,
        state: &AppState,
        build: impl std::future::Future<Output = Result<SnapshotResponse, StatusCode>>,
    ) -> Result<Json<serde_json::Value>, StatusCode> {
        let response = if let Some(key) = self.key {
            let result = state
                .catalog_snapshots
                .cache
                .try_get_with(key, async {
                    let result = build.await.map_err(BuildFailure::Status)?;
                    let value = Arc::new(result.response.0);
                    if result.complete {
                        Ok(value)
                    } else {
                        Err(BuildFailure::Degraded(value))
                    }
                })
                .await;
            let snapshot = match result {
                Ok(value) => value,
                Err(failure) => match failure.as_ref() {
                    BuildFailure::Status(status) => return Err(*status),
                    BuildFailure::Degraded(value) => value.clone(),
                },
            };
            let slice = |items: &[serde_json::Value]| {
                items
                    .iter()
                    .skip(self.pagination.start_index)
                    .take(self.pagination.limit.unwrap_or(usize::MAX))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            if let Some(items) = snapshot.as_array() {
                serde_json::Value::Array(slice(items))
            } else {
                let mut response = snapshot
                    .as_object()
                    .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
                    .iter()
                    .filter(|(key, _)| key.as_str() != "Items")
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<serde_json::Map<_, _>>();
                let items = snapshot["Items"]
                    .as_array()
                    .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
                response.insert("Items".into(), serde_json::Value::Array(slice(items)));
                response.insert(
                    "StartIndex".into(),
                    serde_json::json!(self.pagination.start_index.min(i32::MAX as usize)),
                );
                serde_json::Value::Object(response)
            }
        } else {
            build.await?.response.0
        };
        if let Some(token) = self.token {
            state
                .client_sessions
                .cache_media_response(&token, &response)
                .await;
        }
        Ok(Json(response))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, setup};
    use super::*;

    #[tokio::test]
    async fn a_mutation_fences_inflight_builds_without_invalidating_another_viewer() {
        let (state, _, sessions, _) = setup().await;
        let prepare = |viewer: &str| {
            let mut request = request("/Items?Limit=1", &sessions);
            request.access_scope =
                Some(crate::virtual_library_service::VirtualLibraryAccessScope::new(viewer, []));
            request
        };
        let mut old = prepare("viewer");
        let old = SnapshotRequest::prepare(&state, &mut old, &[], "listing").await;
        let mut other = prepare("other");
        let other = SnapshotRequest::prepare(&state, &mut other, &[], "listing").await;
        let value = |name| SnapshotResponse {
            response: Json(
                serde_json::json!({"Items": [{"Id": name}], "TotalRecordCount": 1, "StartIndex": 0}),
            ),
            complete: true,
        };
        let _ = other
            .serve(&state, async { Ok(value("other")) })
            .await
            .unwrap();
        let started = tokio::sync::Notify::new();
        let release = tokio::sync::Notify::new();
        let old_build = old.serve(&state, async {
            started.notify_one();
            release.notified().await;
            Ok(value("old"))
        });
        let mutation = async {
            started.notified().await;
            state.catalog_snapshots.invalidate_viewer("viewer");
            release.notify_one();
        };
        let (old, ()) = tokio::join!(old_build, mutation);
        assert_eq!(old.unwrap().0["Items"][0]["Id"], "old");
        let mut fresh = prepare("viewer");
        let fresh = SnapshotRequest::prepare(&state, &mut fresh, &[], "listing").await;
        assert_eq!(
            fresh
                .serve(&state, async { Ok(value("new")) })
                .await
                .unwrap()
                .0["Items"][0]["Id"],
            "new"
        );
        let mut other = prepare("other");
        let other = SnapshotRequest::prepare(&state, &mut other, &[], "listing").await;
        assert_eq!(
            other
                .serve(&state, async {
                    panic!("another viewer's snapshot must survive")
                })
                .await
                .unwrap()
                .0["Items"][0]["Id"],
            "other"
        );
    }
}
