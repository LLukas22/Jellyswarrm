use super::services::CatalogService;
use super::{
    library_resolution::CatalogFetchTarget, postprocessing::Pagination,
    request_policy::is_upstream_limited_catalog_request,
};
use crate::application::error::ApplicationError;
use crate::{
    application::catalog::input::CatalogRequest, request_preprocessing::JellyfinAuthorization,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Bounded, token/viewer/target-scoped materialized query results. Moka coalesces
/// concurrent builds; failed or incomplete scans are never inserted.
pub(crate) struct CatalogSnapshots {
    cache: moka::future::Cache<String, Arc<SnapshotResponse>>,
    revisions: Mutex<HashMap<String, u64>>,
}

impl Default for CatalogSnapshots {
    fn default() -> Self {
        Self {
            revisions: Mutex::new(HashMap::new()),
            cache: moka::future::Cache::builder()
                .max_capacity(64 * 1024 * 1024)
                .weigher(|_: &String, value: &Arc<SnapshotResponse>| {
                    value.response.to_string().len().min(u32::MAX as usize) as u32
                })
                .time_to_live(Duration::from_secs(60))
                .build(),
        }
    }
}

impl CatalogSnapshots {
    async fn materialize(
        &self,
        key: String,
        build: impl std::future::Future<Output = Result<SnapshotResponse, ApplicationError>>,
    ) -> Result<Arc<SnapshotResponse>, ApplicationError> {
        let result = self
            .cache
            .try_get_with(key, async {
                let result = build
                    .await
                    .map_err(|error| BuildFailure::Error(Arc::new(error)))?;
                let complete = result.report.is_complete_inventory();
                let value = Arc::new(result);
                if complete {
                    Ok(value)
                } else {
                    Err(BuildFailure::Degraded(value))
                }
            })
            .await;
        match result {
            Ok(value) => Ok(value),
            Err(failure) => match failure.as_ref() {
                BuildFailure::Error(error) => Err(ApplicationError::Shared(error.clone())),
                BuildFailure::Degraded(value) => Ok(value.clone()),
            },
        }
    }

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

pub(crate) struct SnapshotResponse {
    pub response: serde_json::Value,
    pub report: super::scan::CatalogReport,
}

enum BuildFailure {
    Error(Arc<ApplicationError>),
    // Share a degraded response with current waiters, but never cache it.
    Degraded(Arc<SnapshotResponse>),
}

#[cfg(test)]
impl CatalogSnapshots {
    pub(crate) fn expire(&self) {
        self.cache.invalidate_all();
    }
}

pub(crate) struct SnapshotRequest {
    key: Option<String>,
    pagination: Pagination,
    token: Option<String>,
}

fn hash_request_identity(
    hash: &mut Sha256,
    request: &CatalogRequest,
    scope: &str,
    token: Option<&str>,
) {
    hash.update(format!(
        "{scope}|{}|{}|{:?}|{:?}|{:?}|{:?}",
        request.original_request.method(),
        request.query().identity(),
        request.original_request.headers(),
        request.context.access_scope(),
        request.context.user().map(|user| &user.id),
        token,
    ));
    // Origin is HTTP cache metadata, not part of request-independent query intent.
    hash.update(
        request
            .original_request
            .url()
            .origin()
            .ascii_serialization(),
    );
    if let Some(body) = request
        .original_request
        .body()
        .and_then(reqwest::Body::as_bytes)
    {
        hash.update(body);
    }
}

impl SnapshotRequest {
    pub async fn prepare(
        state: &CatalogService,
        request: &CatalogRequest,
        targets: &[CatalogFetchTarget],
        scope: &str,
    ) -> Self {
        let token = request
            .context
            .authorization()
            .and_then(|auth| auth.token_ref())
            .map(str::to_string)
            .or_else(|| {
                JellyfinAuthorization::from_request(&request.original_request)
                    .and_then(|auth| auth.token())
            });
        let query = request.query();
        tracing::debug!(kind = ?query.kind, scope, "Planning catalog materialization");
        let pagination = query.pagination;
        // Bounded endpoints define their own result window. Do not cache across
        // windows or pretend their response is an exhaustive catalog snapshot.
        if is_upstream_limited_catalog_request(request.original_request.url())
            || matches!(query.kind, super::query::CatalogQueryKind::Feed)
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
        let mut hash = Sha256::new();
        let viewer = request
            .context
            .access_scope()
            .as_ref()
            .map(|scope| scope.user_id())
            .or_else(|| request.context.user().map(|user| user.id.as_str()))
            .unwrap_or("anonymous");
        hash.update(state.catalog_snapshots.revision(viewer).to_le_bytes());
        hash_request_identity(&mut hash, request, scope, token.as_deref());
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
                "{:?}|{:?}|{}|{}|{}",
                target.server,
                target.parent,
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

    pub fn materialization_policy(
        &self,
        request: &CatalogRequest,
    ) -> super::request_policy::CatalogRequestPolicy {
        request.query().policy(self.key.is_some())
    }

    pub async fn serve(
        self,
        state: &CatalogService,
        build: impl std::future::Future<Output = Result<SnapshotResponse, ApplicationError>>,
    ) -> Result<serde_json::Value, ApplicationError> {
        let response = if let Some(key) = self.key {
            let snapshot = state.catalog_snapshots.materialize(key, build).await?;
            super::postprocessing::page_materialized_response(&snapshot.response, self.pagination)?
        } else {
            build.await?.response
        };
        crate::application::observation::media_response(
            &state.client_sessions,
            self.token.as_deref(),
            &response,
        )
        .await;
        Ok(response)
    }
}

#[cfg(test)]
mod materialization_tests {
    use super::*;
    use crate::application::catalog::scan::{FetchOutcome, PartialReason, SourceOutcome};

    #[tokio::test]
    async fn coalesced_build_failure_preserves_its_storage_cause() {
        let snapshots = CatalogSnapshots::default();
        let error = match snapshots
            .materialize("failed".into(), async {
                Err(ApplicationError::Storage(sqlx::Error::RowNotFound))
            })
            .await
        {
            Ok(_) => panic!("failed builds cannot be cached"),
            Err(error) => error,
        };
        let mut cause: &(dyn std::error::Error + 'static) = &error;
        while !cause.is::<sqlx::Error>() {
            cause = cause
                .source()
                .expect("storage cause must survive shared cache errors");
        }
        assert_eq!(
            error.detail_status(),
            hyper::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert!(snapshots
            .materialize("failed".into(), async {
                Ok(result(FetchOutcome::Complete))
            })
            .await
            .is_ok());
    }

    fn result(outcome: FetchOutcome) -> SnapshotResponse {
        SnapshotResponse {
            response: serde_json::json!({"Items": [{"Id": "a", "Future": {"kept": true}}, {"Id": "b"}], "TotalRecordCount": 2, "FutureEnvelope": 42}),
            report: crate::application::catalog::scan::CatalogReport::new(
                vec![SourceOutcome {
                    server_id: crate::server_id::ServerId::new(1),
                    parent_id: Some("library".into()),
                    outcome,
                }],
                0,
            ),
        }
    }

    fn request_identity(path: &str, viewer: &str, token: &str) -> String {
        let mut request = reqwest::Request::new(
            reqwest::Method::GET,
            url::Url::parse(&format!("http://proxy{path}")).unwrap(),
        );
        request
            .headers_mut()
            .insert("X-Emby-Token", token.parse().unwrap());
        let context = crate::application::context::RequestContext::new(
            &request,
            None,
            None,
            None,
            Some(crate::virtual_library_service::VirtualLibraryAccessScope::new(viewer, [])),
        );
        let request = CatalogRequest::new(request, context, None);
        let mut hash = Sha256::new();
        hash_request_identity(&mut hash, &request, "library", Some(token));
        hex::encode(hash.finalize())
    }

    #[test]
    fn canonical_keys_preserve_mixed_case_duplicate_sort_precedence() {
        let original = request_identity("/Items?sortBy=AlbumArtist&SortBy=DateCreated,SortName&sortOrder=Ascending&SortOrder=Descending,Ascending&Limit=1", "viewer", "token");
        let same = request_identity("/Items?StartIndex=1&sortOrder=Ascending&SortOrder=Descending,Ascending&sortBy=AlbumArtist&SortBy=DateCreated,SortName", "viewer", "token");
        let reversed = request_identity("/Items?SortBy=DateCreated,SortName&sortBy=AlbumArtist&SortOrder=Descending,Ascending&sortOrder=Ascending", "viewer", "token");
        assert_eq!(original, same);
        assert_ne!(original, reversed);
    }

    #[test]
    fn canonical_keys_share_pages_but_isolate_credentials_viewers_and_extensions() {
        let base = request_identity("/Items?ParentId=p&Limit=2&Future=a", "viewer", "secret-a");
        assert_eq!(
            base,
            request_identity(
                "/Items?Future=a&StartIndex=2&ParentId=p&Limit=7",
                "viewer",
                "secret-a"
            )
        );
        assert_ne!(
            base,
            request_identity("/Items?ParentId=p&Future=a", "other", "secret-a")
        );
        assert_ne!(
            base,
            request_identity("/Items?ParentId=p&Future=a", "viewer", "secret-b")
        );
        assert_ne!(
            base,
            request_identity("/Items?ParentId=p&Future=b", "viewer", "secret-a")
        );
    }

    #[tokio::test]
    async fn degraded_outcomes_survive_materialization_but_are_not_cached() {
        let cache = CatalogSnapshots::default();
        for outcome in [
            FetchOutcome::Partial(PartialReason::Deadline),
            FetchOutcome::BoundedFeed,
        ] {
            let response = cache
                .materialize("query".into(), async { Ok(result(outcome)) })
                .await
                .unwrap();
            assert_eq!(response.report.outcomes()[0].outcome, outcome);
            assert!(cache.cache.get("query").await.is_none());
        }
        cache
            .materialize("query".into(), async { Ok(result(FetchOutcome::Complete)) })
            .await
            .unwrap();
        let hit = cache
            .materialize("query".into(), async {
                panic!("complete result should be reused")
            })
            .await
            .unwrap();
        let page = super::super::postprocessing::page_materialized_response(
            &hit.response,
            Pagination {
                start_index: 0,
                limit: Some(1),
            },
        )
        .unwrap();
        assert_eq!(page["Items"][0]["Future"]["kept"], true);
        assert_eq!(page["FutureEnvelope"], 42);
        assert_eq!(page["TotalRecordCount"], 2);
        assert_eq!(hit.report.outcomes()[0].outcome, FetchOutcome::Complete);
    }
}
