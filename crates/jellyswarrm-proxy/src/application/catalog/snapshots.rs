//! Cache and materialize catalog snapshots with query-aware pagination and refresh policy.
//!
//! A snapshot is the processed listing of one query. Building it scans every
//! backend, so builds run as background tasks: a client that gives up does not
//! cancel the scan, and concurrent requests share one build. A snapshot stays
//! fresh for [`CatalogCacheSettings::fresh_for`]. After that it is still served,
//! up to [`CatalogCacheSettings::max_age`], while a rebuild runs in the
//! background, so a large library is scanned behind the client rather than
//! while it waits. A rebuild that fails or comes back partial keeps the old
//! snapshot, and the next attempt waits another freshness period.
//!
//! Listings keep their items in the shared [`ItemStore`], so two listings of
//! the same items store each item once. The budget charges every shared record
//! once and evicts the least recently used listings until the total fits.
//!
//! Two revisions per viewer fence snapshots. Changes to what a viewer may see
//! or to the library bump the inventory revision, which is part of every key.
//! Watched, favorite and progress writes bump only the user-data revision:
//! queries that filter or sort by user data include it in their key, and other
//! pages are served with their user data refreshed by one batch lookup.
use super::{
    item_store::{ItemStore, StoredListing},
    library_resolution::CatalogFetchTarget,
    postprocessing::Pagination,
};
use crate::application::error::ApplicationError;
use crate::application::media_services::MediaServices;
use crate::config::CatalogCacheSettings;
use crate::{
    application::request::input::OperationRequest, protocol::caller::JellyfinAuthorization,
};
use futures_util::future::{BoxFuture, FutureExt, Shared};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

type BuildOutcome = Result<Arc<Snapshot>, Arc<ApplicationError>>;
type SharedBuild = Shared<BoxFuture<'static, BuildOutcome>>;

/// Bounded, token/viewer/target-scoped materialized query results. Failed or
/// incomplete builds are returned to their waiters but never cached.
pub(crate) struct CatalogSnapshots {
    settings: CatalogCacheSettings,
    store: Arc<ItemStore>,
    cache: Mutex<ListingCache>,
    builds: Arc<Mutex<HashMap<String, SharedBuild>>>,
    revisions: Mutex<HashMap<String, ViewerRevisions>>,
}

#[derive(Default, Clone, Copy)]
struct ViewerRevisions {
    inventory: u64,
    user_data: u64,
}

pub(crate) struct Snapshot {
    pub(super) listing: StoredListing,
    /// How the build went. Kept with the listing for diagnostics and tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) report: super::scan::CatalogReport,
    /// The viewer's user-data revision when the build started.
    user_data_revision: u64,
    /// When a hit should start a rebuild. Pushed back by each attempt, so a
    /// failing rebuild is retried once per freshness period, not per request.
    refresh_at: Mutex<Instant>,
}

/// Cached listings in least-recently-used order. Their items live in the
/// shared store; `listing_bytes` counts only what the listings own.
#[derive(Default)]
struct ListingCache {
    entries: HashMap<String, CachedListing>,
    listing_bytes: usize,
    clock: u64,
}

struct CachedListing {
    snapshot: Arc<Snapshot>,
    inserted: Instant,
    last_used: u64,
    bytes: usize,
}

impl ListingCache {
    fn remove(&mut self, key: &str) -> Option<Arc<Snapshot>> {
        let removed = self.entries.remove(key)?;
        self.listing_bytes -= removed.bytes;
        Some(removed.snapshot)
    }

    fn least_recently_used(&self, except: &str) -> Option<String> {
        self.entries
            .iter()
            .filter(|(key, _)| key.as_str() != except)
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())
    }

    fn take_all(&mut self) -> Vec<Arc<Snapshot>> {
        self.listing_bytes = 0;
        self.entries
            .drain()
            .map(|(_, entry)| entry.snapshot)
            .collect()
    }
}

fn snapshot_len(cache: &ListingCache, key: &str) -> usize {
    cache
        .entries
        .get(key)
        .map_or(0, |entry| entry.snapshot.listing.len())
}

impl Default for CatalogSnapshots {
    fn default() -> Self {
        Self::new(CatalogCacheSettings::default())
    }
}

impl CatalogSnapshots {
    pub(crate) fn new(settings: CatalogCacheSettings) -> Self {
        Self {
            settings,
            store: Arc::default(),
            cache: Mutex::default(),
            builds: Arc::default(),
            revisions: Mutex::default(),
        }
    }

    pub(super) fn item_store(&self) -> Arc<ItemStore> {
        Arc::clone(&self.store)
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, ListingCache> {
        self.cache.lock().expect("snapshot cache poisoned")
    }

    /// A cached listing that has not outlived `max_age`.
    fn cached(&self, key: &str) -> Option<Arc<Snapshot>> {
        let mut cache = self.cache();
        cache.clock += 1;
        let clock = cache.clock;
        let max_age = self.settings.max_age;
        let entry = cache.entries.get_mut(key)?;
        if entry.inserted.elapsed() >= max_age {
            let expired = cache.remove(key);
            drop(cache);
            drop(expired);
            return None;
        }
        entry.last_used = clock;
        Some(Arc::clone(&entry.snapshot))
    }

    /// Retained bytes charged against the budget: shared records once, plus
    /// what each cached listing owns.
    fn retained_bytes(&self) -> usize {
        // Records of uncached listings that are still being served count too,
        // until their last reader finishes.
        self.cache().listing_bytes + self.store.stats().bytes
    }

    /// Cache a complete listing, evicting the least recently used ones until
    /// the budget fits. Victims are dropped outside the cache lock, and the
    /// budget is checked again against what their release actually freed:
    /// records shared with other listings or held by readers stay.
    fn admit(&self, key: String, snapshot: Arc<Snapshot>) {
        let budget = usize::try_from(self.settings.max_bytes).unwrap_or(usize::MAX);
        let bytes = snapshot.listing.own_bytes();
        // A listing that cannot fit even in an empty cache must not flush
        // every other viewer's listings first.
        if snapshot.listing.standalone_bytes() > budget {
            tracing::info!(
                items = snapshot.listing.len(),
                bytes = snapshot.listing.standalone_bytes(),
                budget,
                "Catalog listing exceeds the cache budget and is not retained"
            );
            return;
        }
        let replaced = {
            let mut cache = self.cache();
            cache.clock += 1;
            let clock = cache.clock;
            let replaced = cache.remove(&key);
            cache.listing_bytes += bytes;
            cache.entries.insert(
                key.clone(),
                CachedListing {
                    snapshot,
                    inserted: Instant::now(),
                    last_used: clock,
                    bytes,
                },
            );
            replaced
        };
        drop(replaced);
        self.purge_expired();
        let stats = self.store.stats();
        tracing::debug!(
            items = snapshot_len(&self.cache(), &key),
            shared_records = stats.records,
            shared_bytes = stats.bytes,
            budget,
            "Cached catalog listing"
        );
        while self.retained_bytes() > budget {
            let victim = {
                let mut cache = self.cache();
                let Some(victim) = cache.least_recently_used(&key) else {
                    break;
                };
                cache.remove(&victim)
            };
            drop(victim);
        }
        if self.retained_bytes() > budget {
            // Only pinned readers or uncached builds still hold the excess.
            tracing::debug!(
                retained = self.retained_bytes(),
                budget,
                "Catalog cache is over budget until active readers finish"
            );
        }
    }

    fn purge_expired(&self) {
        let max_age = self.settings.max_age;
        let expired = {
            let mut cache = self.cache();
            let keys = cache
                .entries
                .iter()
                .filter(|(_, entry)| entry.inserted.elapsed() >= max_age)
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| cache.remove(&key))
                .collect::<Vec<_>>()
        };
        drop(expired);
    }

    async fn materialize(
        self: &Arc<Self>,
        key: String,
        user_data_revision: u64,
        build: impl std::future::Future<Output = Result<SnapshotResponse, ApplicationError>>
            + Send
            + 'static,
    ) -> Result<Arc<Snapshot>, ApplicationError> {
        if let Some(snapshot) = self.cached(&key) {
            if self.claim_refresh(&snapshot) {
                // Serve the stale listing now. The rebuild replaces it for
                // later requests, and a failed or partial one leaves it alone.
                let stale = Arc::clone(&snapshot);
                let fresh_for = self.settings.fresh_for;
                drop(self.start_build(key, user_data_revision, async move {
                    let result = build.await;
                    *stale.refresh_at.lock().expect("snapshot refresh poisoned") =
                        Instant::now() + fresh_for;
                    result
                }));
            }
            return Ok(snapshot);
        }
        self.start_build(key, user_data_revision, build)
            .await
            .map_err(ApplicationError::Shared)
    }

    /// Whether this hit should rebuild the snapshot. Claiming pushes the next
    /// attempt one freshness period out.
    fn claim_refresh(&self, snapshot: &Snapshot) -> bool {
        let now = Instant::now();
        let mut refresh_at = snapshot
            .refresh_at
            .lock()
            .expect("snapshot refresh poisoned");
        if now < *refresh_at {
            return false;
        }
        *refresh_at = now + self.settings.fresh_for;
        true
    }

    /// Join the running build for `key`, or start one. The build runs to
    /// completion even if every waiter goes away.
    fn start_build(
        self: &Arc<Self>,
        key: String,
        user_data_revision: u64,
        build: impl std::future::Future<Output = Result<SnapshotResponse, ApplicationError>>
            + Send
            + 'static,
    ) -> SharedBuild {
        let mut builds = self.builds.lock().expect("snapshot builds poisoned");
        if let Some(running) = builds.get(&key) {
            return running.clone();
        }
        let snapshots = Arc::clone(self);
        let store = Arc::clone(&self.store);
        // Unregisters the build when the task ends, even by panicking, so a
        // failed build is never handed to later requests.
        let slot = BuildSlot {
            builds: Arc::clone(&self.builds),
            key: key.clone(),
        };
        let fresh_for = self.settings.fresh_for;
        let task = tokio::spawn(async move {
            let outcome = match build
                .await
                .and_then(|SnapshotResponse { response, report }| {
                    Ok((store.store_listing(response)?, report))
                }) {
                Ok((listing, report)) => {
                    let complete = report.is_complete_inventory();
                    let snapshot = Arc::new(Snapshot {
                        listing,
                        report,
                        user_data_revision,
                        refresh_at: Mutex::new(Instant::now() + fresh_for),
                    });
                    if complete && snapshots.settings.max_bytes > 0 {
                        snapshots.admit(slot.key.clone(), Arc::clone(&snapshot));
                    }
                    Ok(snapshot)
                }
                Err(error) => Err(Arc::new(error)),
            };
            drop(slot);
            outcome
        });
        let build = async move {
            task.await
                .unwrap_or_else(|error| Err(Arc::new(ApplicationError::Internal(error.into()))))
        }
        .boxed()
        .shared();
        builds.insert(key, build.clone());
        build
    }

    fn revisions(&self, viewer: &str) -> ViewerRevisions {
        *self
            .revisions
            .lock()
            .expect("snapshot revisions poisoned")
            .entry(viewer.to_owned())
            .or_default()
    }

    /// The viewer's inventory or access changed. A revision also fences off
    /// builds that were in flight during the write.
    pub(crate) fn invalidate_viewer(&self, viewer: &str) {
        let mut revisions = self.revisions.lock().expect("snapshot revisions poisoned");
        let revisions = revisions.entry(viewer.to_string()).or_default();
        revisions.inventory += 1;
        revisions.user_data += 1;
    }

    /// Only the viewer's watched, favorite or progress state changed. Listings
    /// keep their inventory and refresh the user data of the page they serve.
    pub(crate) fn user_data_changed(&self, viewer: &str) {
        let mut revisions = self.revisions.lock().expect("snapshot revisions poisoned");
        revisions.entry(viewer.to_string()).or_default().user_data += 1;
    }

    /// Registry changes affect every viewer. Registering revisions at read time
    /// also fences builders that started before this mutation, not just entries
    /// already present in the cache.
    pub(crate) fn invalidate_all(&self) {
        let mut revisions = self.revisions.lock().expect("snapshot revisions poisoned");
        for revision in revisions.values_mut() {
            revision.inventory += 1;
            revision.user_data += 1;
        }
        drop(revisions);
        let dropped = self.cache().take_all();
        drop(dropped);
    }

    /// Retained size of the shared item records.
    #[cfg(test)]
    pub(crate) fn store_stats(&self) -> super::item_store::StoreStats {
        self.store.stats()
    }
}

struct BuildSlot {
    builds: Arc<Mutex<HashMap<String, SharedBuild>>>,
    key: String,
}

impl Drop for BuildSlot {
    fn drop(&mut self) {
        // A panic elsewhere must not turn into a second one while unwinding.
        if let Ok(mut builds) = self.builds.lock() {
            builds.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod registry_invalidation_tests {
    use super::*;
    #[test]
    fn registry_mutations_fence_all_registered_viewers_including_inflight_builds() {
        let snapshots = CatalogSnapshots::default();
        let first = snapshots.revisions("first").inventory;
        let second = snapshots.revisions("second").inventory;
        snapshots.invalidate_all();
        assert_ne!(snapshots.revisions("first").inventory, first);
        assert_ne!(snapshots.revisions("second").inventory, second);
    }
}

#[cfg(test)]
impl CatalogSnapshots {
    pub(crate) fn expire(&self) {
        let dropped = self.cache().take_all();
        drop(dropped);
    }

    /// Make every cached snapshot due for a rebuild.
    pub(crate) fn age(&self) {
        let now = Instant::now();
        for entry in self.cache().entries.values() {
            *entry.snapshot.refresh_at.lock().unwrap() = now;
        }
    }

    pub(crate) fn cached_for_test(&self, key: &str) -> Option<Arc<Snapshot>> {
        self.cached(key)
    }

    /// Wait until no build is running.
    pub(crate) async fn settle(&self) {
        loop {
            let running = self
                .builds
                .lock()
                .unwrap()
                .values()
                .cloned()
                .collect::<Vec<_>>();
            if running.is_empty() {
                return;
            }
            for build in running {
                let _ = build.await;
            }
            // The finished task unregisters itself just after resolving.
            tokio::task::yield_now().await;
        }
    }
}

/// A built listing can already own compact records, carry typed items for
/// incremental conversion into records, or hold finished JSON.
pub(crate) enum SnapshotBody {
    Json(serde_json::Value),
    Items(crate::protocol::models::ItemsResponseVariants),
    Stored(StoredListing),
}

impl SnapshotBody {
    fn into_json(self) -> Result<serde_json::Value, ApplicationError> {
        match self {
            Self::Json(value) => Ok(value),
            Self::Stored(listing) => Ok(listing.page(Pagination::unbounded())),
            Self::Items(items) => {
                serde_json::to_value(items).map_err(ApplicationError::Serialization)
            }
        }
    }
}

impl From<serde_json::Value> for SnapshotBody {
    fn from(value: serde_json::Value) -> Self {
        Self::Json(value)
    }
}

pub(crate) struct SnapshotResponse {
    pub(crate) response: SnapshotBody,
    pub(crate) report: super::scan::CatalogReport,
}

pub(crate) struct SnapshotRequest {
    key: Option<String>,
    pagination: Pagination,
    token: Option<String>,
    viewer: String,
    user_data_revision: u64,
    /// Refreshes the user data of a served page when the snapshot predates a
    /// watched or favorite change. `None` when the key already covers it.
    hydration: Option<OperationRequest>,
}

fn hash_request_identity(
    hash: &mut Sha256,
    request: &OperationRequest,
    scope: &str,
    token: Option<&str>,
) {
    hash.update(format!(
        "{scope}|{}|{}|{:?}|{:?}|{:?}|{:?}",
        request.original_request.method(),
        request.catalog_query().identity(),
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

/// Whether a query's membership or order depends on the viewer's watched,
/// favorite or rating state. Such a listing cannot be patched page by page.
fn depends_on_user_data(query: &super::query::CatalogQuery) -> bool {
    const FILTERS: &[&str] = &[
        "IsPlayed",
        "IsUnplayed",
        "IsFavorite",
        "IsResumable",
        "Likes",
        "Dislikes",
        "IsFavoriteOrLikes",
    ];
    const SORTS: &[&str] = &[
        "DatePlayed",
        "SeriesDatePlayed",
        "PlayCount",
        "IsFavoriteOrLiked",
        "IsPlayed",
        "IsUnplayed",
    ];
    let listed = |value: &str, names: &[&str]| {
        value.split(',').any(|value| {
            names
                .iter()
                .any(|name| value.trim().eq_ignore_ascii_case(name))
        })
    };
    query.parameters().iter().any(|(key, value)| {
        (key.eq_ignore_ascii_case("Filters") && listed(value, FILTERS))
            || (key.eq_ignore_ascii_case("SortBy") && listed(value, SORTS))
            || ["IsPlayed", "IsFavorite", "IsLiked", "IsResumable"]
                .iter()
                .any(|name| key.eq_ignore_ascii_case(name))
    })
}

impl SnapshotRequest {
    pub(crate) async fn prepare(
        state: &MediaServices,
        request: &OperationRequest,
        targets: &[CatalogFetchTarget],
        scope: &str,
    ) -> Self {
        let token = request
            .context
            .authorization()
            .and_then(|auth| auth.token_ref())
            .map(str::to_string)
            .or_else(|| {
                JellyfinAuthorization::from_headers_url(
                    request.original_request.headers(),
                    request.original_request.url(),
                )
                .and_then(|auth| auth.token())
            });
        let query = request.catalog_query();
        let endpoint = query.endpoint();
        tracing::debug!(?endpoint, scope, "Planning catalog materialization");
        let pagination = query.pagination;
        let viewer = request
            .context
            .access_scope()
            .map(|scope| scope.user_id())
            .or_else(|| request.context.user().map(|user| user.id.as_str()))
            .unwrap_or("anonymous")
            .to_string();
        let revisions = state.catalog_snapshots.revisions(&viewer);
        // Bounded endpoints and rows define their own result window. Do not
        // cache across windows or pretend their response is an exhaustive
        // catalog snapshot.
        if endpoint.is_feed() || endpoint.is_library_root() || query.policy(false).is_bounded_row()
        {
            return Self {
                key: None,
                pagination,
                token,
                viewer,
                user_data_revision: revisions.user_data,
                hydration: None,
            };
        }
        let hydration = (endpoint == super::query::CatalogEndpoint::Items
            && pagination
                .limit
                .is_some_and(|limit| limit <= MAX_REFRESHED_PAGE)
            && !depends_on_user_data(&query))
        .then(|| {
            Some(OperationRequest {
                original_request: request.original_request.try_clone()?,
                context: request.context.clone(),
                body_analysis: None,
            })
        })
        .flatten();
        let mut hash = Sha256::new();
        hash.update(revisions.inventory.to_le_bytes());
        if hydration.is_none() {
            hash.update(revisions.user_data.to_le_bytes());
        }
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
            viewer,
            user_data_revision: revisions.user_data,
            hydration,
        }
    }

    pub(super) fn is_materialized(&self) -> bool {
        self.key.is_some()
    }

    pub(crate) fn materialization_policy(
        &self,
        request: &OperationRequest,
    ) -> super::request_policy::CatalogRequestPolicy {
        request.catalog_query().policy(self.key.is_some())
    }

    /// `build` runs as its own task, so it owns everything it uses.
    pub(crate) async fn serve(
        self,
        state: &MediaServices,
        build: impl std::future::Future<Output = Result<SnapshotResponse, ApplicationError>>
            + Send
            + 'static,
    ) -> Result<serde_json::Value, ApplicationError> {
        let response = if let Some(key) = self.key {
            let snapshot = state
                .catalog_snapshots
                .materialize(key, self.user_data_revision, build)
                .await?;
            let mut page = snapshot.listing.page(self.pagination);
            if let Some(template) = self.hydration {
                let current = state.catalog_snapshots.revisions(&self.viewer).user_data;
                if snapshot.user_data_revision != current {
                    refresh_user_data(state, template, &mut page).await;
                }
            }
            page
        } else {
            build.await?.response.into_json()?
        };
        if let Some(token) = self.token.as_deref() {
            state
                .client_sessions
                .cache_media_response(token, &response)
                .await;
        }
        Ok(response)
    }
}

/// Pages up to this size have their user data refreshed after a watched or
/// favorite change. Larger or unlimited pages rebuild instead: refreshing them
/// would take many lookups per request.
const MAX_REFRESHED_PAGE: usize = 300;
/// IDs per refresh lookup, which keeps backend URLs well under common
/// request-line limits.
const REFRESH_LOOKUP_IDS: usize = 100;

/// Replace the `UserData` of a page's items with the backends' current state,
/// using batch `Ids` lookups. Membership and order stay as in the snapshot.
/// On failure the page keeps its snapshot user data rather than failing.
async fn refresh_user_data(
    state: &MediaServices,
    template: OperationRequest,
    page: &mut serde_json::Value,
) {
    let items = match page {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(envelope) => match envelope.get_mut("Items") {
            Some(serde_json::Value::Array(items)) => items,
            _ => return,
        },
        _ => return,
    };
    let url = template.original_request.url();
    if url
        .query_pairs()
        .any(|(key, value)| key.eq_ignore_ascii_case("EnableUserData") && value == "false")
    {
        return;
    }
    let ids = items
        .iter()
        .filter_map(|item| item.get("Id")?.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let kept = url
        .query_pairs()
        .filter(|(key, _)| {
            ["UserId", "api_key", "ApiKey"]
                .iter()
                .any(|name| key.eq_ignore_ascii_case(name))
        })
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let lookups = ids.chunks(REFRESH_LOOKUP_IDS).map(|ids| {
        let mut lookup = template
            .original_request
            .try_clone()
            .map(|original_request| OperationRequest {
                original_request,
                context: template.context.clone(),
                body_analysis: None,
            });
        if let Some(lookup) = &mut lookup {
            lookup
                .original_request
                .url_mut()
                .query_pairs_mut()
                .clear()
                .extend_pairs(&kept)
                .append_pair("Ids", &ids.join(","))
                .append_pair("EnableImages", "false")
                .append_pair("EnableUserData", "true");
        }
        async move {
            let lookup = lookup.ok_or(ApplicationError::MissingIdentity("cloneable refresh"))?;
            match super::item_lookup::RequestedItems::resolve(state, &lookup).await? {
                Some(requested) => requested.fetch(state, lookup).await.map(Some),
                None => Ok(None),
            }
        }
    });
    let mut user_data = HashMap::new();
    for fresh in futures_util::future::join_all(lookups).await {
        let fresh = match fresh {
            Ok(Some(fresh)) => fresh,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!("Serving snapshot user data; refreshing it failed: {error:?}");
                continue;
            }
        };
        let fresh_items = fresh
            .get("Items")
            .and_then(serde_json::Value::as_array)
            .or_else(|| fresh.as_array());
        user_data.extend(fresh_items.into_iter().flatten().filter_map(|item| {
            Some((
                item.get("Id")?.as_str()?.to_string(),
                item.get("UserData")?.clone(),
            ))
        }));
    }
    for item in items.iter_mut() {
        let Some(id) = item.get("Id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if let Some(user_data) = user_data.remove(id) {
            item["UserData"] = user_data;
        }
    }
}

#[cfg(test)]
mod materialization_tests {
    use super::*;
    use crate::application::catalog::scan::{FetchOutcome, PartialReason, SourceOutcome};
    use std::time::Duration;

    #[tokio::test]
    async fn coalesced_build_failure_preserves_its_storage_cause() {
        let snapshots = Arc::new(CatalogSnapshots::default());
        let error = match snapshots
            .materialize("failed".into(), 0, async {
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

        assert!(snapshots
            .materialize("failed".into(), 0, async {
                Ok(result(FetchOutcome::Complete))
            })
            .await
            .is_ok());
    }

    fn result(outcome: FetchOutcome) -> SnapshotResponse {
        SnapshotResponse {
            response: serde_json::json!({"Items": [{"Id": "a", "Future": {"kept": true}}, {"Id": "b"}], "TotalRecordCount": 2, "FutureEnvelope": 42}).into(),
            report: crate::application::catalog::scan::CatalogReport::new(
                vec![SourceOutcome {
                    server_id: crate::domain::ids::ServerId::new(1),
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
        let context = crate::application::request::context::RequestContext::new(
            &request,
            None,
            None,
            None,
            Some(crate::libraries::service::VirtualLibraryAccessScope::new(
                viewer,
                [],
            )),
        );
        let request = OperationRequest {
            original_request: request,
            context,
            body_analysis: None,
        };
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
        let cache = Arc::new(CatalogSnapshots::default());
        for outcome in [
            FetchOutcome::Partial(PartialReason::Deadline),
            FetchOutcome::BoundedFeed,
        ] {
            let response = cache
                .materialize("query".into(), 0, async move { Ok(result(outcome)) })
                .await
                .unwrap();
            assert_eq!(response.report.outcomes()[0].outcome, outcome);
            assert!(cache.cached_for_test("query").is_none());
        }
        cache
            .materialize("query".into(), 0, async {
                Ok(result(FetchOutcome::Complete))
            })
            .await
            .unwrap();
        let hit = cache
            .materialize("query".into(), 0, async {
                panic!("complete result should be reused")
            })
            .await
            .unwrap();
        let page = hit.listing.page(Pagination {
            start_index: 0,
            limit: Some(1),
        });
        assert_eq!(page["Items"][0]["Future"]["kept"], true);
        assert_eq!(page["FutureEnvelope"], 42);
        assert_eq!(page["TotalRecordCount"], 2);
        assert_eq!(hit.report.outcomes()[0].outcome, FetchOutcome::Complete);
    }

    #[tokio::test]
    async fn stale_snapshots_are_served_while_one_rebuild_runs() {
        let cache = Arc::new(CatalogSnapshots::default());
        cache
            .materialize("query".into(), 0, async {
                Ok(result(FetchOutcome::Complete))
            })
            .await
            .unwrap();
        cache.age();
        let release = Arc::new(tokio::sync::Notify::new());
        let builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..3 {
            let (release, builds) = (release.clone(), builds.clone());
            let stale = cache
                .materialize("query".into(), 0, async move {
                    builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    release.notified().await;
                    let mut fresh = result(FetchOutcome::Complete);
                    let SnapshotBody::Json(response) = &mut fresh.response else {
                        unreachable!("test listings are JSON")
                    };
                    response["FutureEnvelope"] = 43.into();
                    Ok(fresh)
                })
                .await
                .unwrap();
            assert_eq!(stale.listing.envelope().unwrap()["FutureEnvelope"], 42);
        }
        tokio::task::yield_now().await;
        assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 1);
        release.notify_one();
        cache.settle().await;
        let fresh = cache
            .materialize("query".into(), 0, async {
                panic!("fresh snapshot is reused")
            })
            .await
            .unwrap();
        assert_eq!(fresh.listing.envelope().unwrap()["FutureEnvelope"], 43);
    }

    #[tokio::test]
    async fn degraded_rebuilds_keep_the_stale_complete_snapshot() {
        let cache = Arc::new(CatalogSnapshots::default());
        cache
            .materialize("query".into(), 0, async {
                Ok(result(FetchOutcome::Complete))
            })
            .await
            .unwrap();
        cache.age();
        cache
            .materialize("query".into(), 0, async {
                Ok(result(FetchOutcome::Partial(PartialReason::Deadline)))
            })
            .await
            .unwrap();
        cache.settle().await;
        let kept = cache.cached_for_test("query").unwrap();
        assert_eq!(kept.report.outcomes()[0].outcome, FetchOutcome::Complete);
        // A backend that keeps failing must not cost a full scan per request:
        // the next attempt waits another freshness period.
        cache
            .materialize("query".into(), 0, async {
                panic!("rebuilds are not retried on every request")
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_panicking_build_does_not_poison_its_query() {
        let cache = Arc::new(CatalogSnapshots::default());
        let panicked = cache
            .materialize("query".into(), 0, async {
                if true {
                    panic!("build bug");
                }
                Ok(result(FetchOutcome::Complete))
            })
            .await;
        assert!(panicked.is_err());
        cache
            .materialize("query".into(), 0, async {
                Ok(result(FetchOutcome::Complete))
            })
            .await
            .expect("the next request builds again");
    }

    #[tokio::test]
    async fn a_zero_sized_cache_still_coalesces_but_keeps_nothing() {
        let cache = Arc::new(CatalogSnapshots::new(CatalogCacheSettings {
            max_bytes: 0,
            ..CatalogCacheSettings::default()
        }));
        cache
            .materialize("query".into(), 0, async {
                Ok(result(FetchOutcome::Complete))
            })
            .await
            .unwrap();
        assert!(cache.cached_for_test("query").is_none());
    }

    fn listing(ids: impl Iterator<Item = usize>) -> SnapshotResponse {
        let items = ids
            .map(|id| serde_json::json!({"Id": id.to_string(), "Overview": "x".repeat(200)}))
            .collect::<Vec<_>>();
        let mut complete = result(FetchOutcome::Complete);
        complete.response =
            serde_json::json!({"TotalRecordCount": items.len(), "Items": items}).into();
        complete
    }

    async fn build(cache: &Arc<CatalogSnapshots>, key: &str, response: SnapshotResponse) {
        cache
            .materialize(key.into(), 0, async move { Ok(response) })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn shared_items_are_charged_once_and_least_recently_used_listings_go_first() {
        let probe = Arc::new(CatalogSnapshots::default());
        build(&probe, "probe", listing(0..100)).await;
        let one = probe.retained_bytes();
        drop(probe);

        let budget = |factor: usize| {
            Arc::new(CatalogSnapshots::new(CatalogCacheSettings {
                max_bytes: (one * factor / 4) as u64,
                ..CatalogCacheSettings::default()
            }))
        };

        // Another order of the same items costs only its own entry list.
        let cache = budget(7);
        build(&cache, "by-name", listing(0..100)).await;
        build(&cache, "reversed", listing((0..100).rev())).await;
        assert!(cache.cached_for_test("by-name").is_some());
        assert!(cache.cached_for_test("reversed").is_some());
        assert_eq!(cache.store_stats().records, 100);
        assert!(cache.retained_bytes() < one * 5 / 4);
        // Evicting "reversed" frees only its entry list, so eviction goes on
        // until the shared records are released too.
        build(&cache, "other", listing(100..200)).await;
        assert!(cache.cached_for_test("reversed").is_none());
        assert!(cache.cached_for_test("by-name").is_none());
        assert!(cache.cached_for_test("other").is_some());
        assert_eq!(cache.store_stats().records, 100);
        assert!(cache.retained_bytes() <= one * 7 / 4);

        // Distinct listings: the least recently used one goes first.
        let cache = budget(10);
        build(&cache, "a", listing(0..100)).await;
        build(&cache, "b", listing(100..200)).await;
        assert!(cache.cached_for_test("a").is_some());
        build(&cache, "c", listing(200..300)).await;
        assert!(cache.cached_for_test("b").is_none());
        assert!(cache.cached_for_test("a").is_some());
        assert!(cache.cached_for_test("c").is_some());

        // A listing larger than the whole budget is served but flushes nothing.
        let oversized = cache
            .materialize("oversized".into(), 0, async { Ok(listing(300..600)) })
            .await
            .unwrap();
        assert_eq!(oversized.listing.len(), 300);
        assert!(cache.cached_for_test("oversized").is_none());
        assert!(cache.cached_for_test("a").is_some());
        assert!(cache.cached_for_test("c").is_some());
        drop(oversized);
        assert_eq!(cache.store_stats().records, 200);

        cache.invalidate_all();
        assert_eq!(cache.store_stats(), Default::default());
        assert_eq!(cache.retained_bytes(), 0);
    }

    #[tokio::test]
    async fn expired_listings_release_their_items() {
        let cache = Arc::new(CatalogSnapshots::new(CatalogCacheSettings {
            fresh_for: Duration::from_millis(5),
            max_age: Duration::from_millis(20),
            ..CatalogCacheSettings::default()
        }));
        build(&cache, "query", listing(0..10)).await;
        assert_eq!(cache.store_stats().records, 10);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(cache.cached_for_test("query").is_none());
        assert_eq!(cache.store_stats(), Default::default());
    }

    #[tokio::test]
    async fn a_reader_keeps_an_evicted_listing_readable() {
        let cache = Arc::new(CatalogSnapshots::default());
        build(&cache, "query", listing(0..10)).await;
        let reader = cache.cached_for_test("query").unwrap();
        cache.invalidate_all();
        assert_eq!(cache.store_stats().records, 10);
        assert_eq!(
            reader.listing.page(Pagination::unbounded())["Items"][9]["Id"],
            "9"
        );
        drop(reader);
        assert_eq!(cache.store_stats(), Default::default());
    }

    #[tokio::test]
    async fn abandoned_builds_still_complete_and_are_cached() {
        let cache = Arc::new(CatalogSnapshots::default());
        let abandoned = tokio::time::timeout(
            Duration::from_millis(10),
            cache.materialize("query".into(), 0, async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(result(FetchOutcome::Complete))
            }),
        )
        .await;
        assert!(abandoned.is_err(), "the client gave up first");
        cache.settle().await;
        assert!(cache.cached_for_test("query").is_some());
    }

    #[test]
    fn user_data_queries_are_recognized() {
        let depends = |query: &str| {
            depends_on_user_data(&super::super::query::CatalogQuery::from_url(
                &url::Url::parse(&format!("http://proxy/Items?{query}")).unwrap(),
            ))
        };
        for query in [
            "Filters=IsUnplayed",
            "filters=IsFolder,IsFavorite",
            "SortBy=DatePlayed",
            "sortBy=seriesdateplayed",
            "SortBy=SortName,IsFavoriteOrLiked",
            "IsPlayed=false",
            "isFavorite=true",
        ] {
            assert!(depends(query), "{query}");
        }
        for query in [
            "Filters=IsFolder",
            "SortBy=SortName",
            "ParentId=library&Recursive=true",
        ] {
            assert!(!depends(query), "{query}");
        }
    }
}
