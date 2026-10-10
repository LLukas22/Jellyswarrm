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
use std::{sync::Arc, time::Duration};

/// Bounded, token/viewer/target-scoped materialized query results. Moka coalesces
/// concurrent builds; failed or incomplete scans are never inserted.
pub(crate) struct CatalogSnapshots {
    cache: moka::future::Cache<String, Arc<serde_json::Value>>,
}

impl Default for CatalogSnapshots {
    fn default() -> Self {
        Self {
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
        // Bounded endpoints define their own result window. Do not cache across
        // windows or pretend their response is an exhaustive catalog snapshot.
        if is_upstream_limited_catalog_request(request.original_request.url())
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
        build: impl std::future::Future<Output = Result<Json<serde_json::Value>, StatusCode>>,
    ) -> Result<Json<serde_json::Value>, StatusCode> {
        let response = if let Some(key) = self.key {
            let snapshot = state
                .catalog_snapshots
                .cache
                .try_get_with(key, async {
                    build.await.map(|Json(value)| Arc::new(value))
                })
                .await
                .map_err(|status| *status)?;
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
            build.await?.0
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
