//! Fetch catalog pages from authorized backends with bounded scans and completeness evidence.
use std::{future::Future, sync::Arc, time::Duration};

use crate::application::error::{ApplicationError, FailureStage};
use tokio::task::JoinSet;
use tracing::{debug, error, trace, warn};

use crate::application::media_services::MediaServices;
use crate::{
    application::request::response::response_json_to_payload,
    auth::service::AuthorizationSession,
    domain::ids::{ServerId, VirtualItemId},
    libraries::service::DiscoveredLibrary,
    protocol::caller::JellyfinAuthorization,
    protocol::models::{ItemsResponseVariants, ItemsResponseWithCount},
    servers::storage::Server,
    translation::response_processor::ResponseProcessingProfile,
};

use super::{
    item_store::StoredListing,
    library_resolution::presentable_library_collection_type,
    library_resolution::CatalogFetchTarget,
    postprocessing::{Pagination, ResponseShape, ServerItems, SortKey, SortPolicy},
    query::CatalogEndpoint,
    request_policy::{
        normalize_upstream_pagination, replace_aggregate_parent_id, set_upstream_page,
        CatalogRequestPolicy, UPSTREAM_PAGE_SIZE, UPSTREAM_SCAN_CONCURRENCY,
    },
    scan::{CatalogReport, FetchOutcome, PartialReason, ScanProgress, SourceOutcome},
};
use crate::protocol::url::replace_path_id;

pub(crate) struct FetchedCatalog<T = ServerItems> {
    pub(crate) server_items: Vec<FetchedServerItems<T>>,
    pub(crate) response_shape: ResponseShape,
    pub(crate) report: CatalogReport,
}

fn source_outcomes(targets: &[CatalogFetchTarget]) -> Vec<SourceOutcome> {
    targets
        .iter()
        .map(|target| SourceOutcome {
            server_id: target.server.id,
            parent_id: target.parent.as_ref().map(|parent| parent.backend.clone()),
            outcome: FetchOutcome::Partial(PartialReason::Failed),
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(crate) enum FetchMode {
    Listing,
    VirtualLibrary,
}

/// Backend IDs have not been translated. Only inventory grouping or the
/// translation boundary can consume these responses.
pub(super) struct RawServerItems {
    pub(super) response: ItemsResponseVariants,
    pub(super) server: Server,
}

pub(crate) struct FetchedServerItems<T = ServerItems> {
    pub(crate) server_items: T,
    pub(crate) outcome: FetchOutcome,
    pub(crate) source_parent_id: Option<crate::domain::ids::BackendItemId>,
}

/// Federates `/Shows/{seriesId}/Seasons|Episodes` for a collapsed (aggregate)
/// show by fanning out the path ID to each member's virtual series ID, which
/// the upstream builder translates once into that member's backend identity.
pub(super) async fn fetch_show_catalog(
    state: &MediaServices,
    original_request: &reqwest::Request,
    policy: &CatalogRequestPolicy,
    targets: Vec<CatalogFetchTarget>,
    mut failures: usize,
) -> Result<FetchedCatalog, ApplicationError> {
    let unavailable_targets = failures;
    let mut join_set = JoinSet::new();
    let mut outcomes = source_outcomes(&targets);
    let season_id = original_request
        .url()
        .query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("SeasonId"))
        .map(|(_, id)| VirtualItemId::from(id.as_ref()));
    let season_mappings = if let Some(id) = season_id.as_ref() {
        if let Some(group) = state
            .media_storage
            .get_media_version_group(id)
            .await
            .map_err(ApplicationError::Storage)?
        {
            Some(
                state
                    .media_storage
                    .get_media_version_members(group.id)
                    .await
                    .map_err(ApplicationError::Storage)?
                    .into_iter()
                    .map(|member| member.mapping)
                    .collect::<Vec<_>>(),
            )
        } else {
            Some(
                state
                    .media_storage
                    .get_media_mapping_by_virtual(id)
                    .await
                    .map_err(ApplicationError::Storage)?
                    .into_iter()
                    .collect(),
            )
        }
    } else {
        None
    };

    for (index, target) in targets.into_iter().enumerate() {
        let season_mapping = if let Some(mappings) = &season_mappings {
            let Some(mapping) = mappings
                .iter()
                .find(|mapping| mapping.server_id == target.server.id)
            else {
                continue;
            };
            Some(mapping)
        } else {
            None
        };
        let Some(mut request) = original_request.try_clone() else {
            error!("Failed to clone request for server: {}", target.server.name);
            failures += 1;
            continue;
        };
        if let Some(mapping) = season_mapping {
            let pairs = request
                .url()
                .query_pairs()
                .map(|(key, value)| {
                    let value = if key.eq_ignore_ascii_case("SeasonId") {
                        mapping.virtual_media_id.as_str().to_owned()
                    } else {
                        value.into_owned()
                    };
                    (key.into_owned(), value)
                })
                .collect::<Vec<_>>();
            request
                .url_mut()
                .query_pairs_mut()
                .clear()
                .extend_pairs(pairs);
        }
        policy.prepare_upstream(request.url_mut(), true);
        let source_parent_id = target.parent.as_ref().map(|parent| parent.backend.clone());
        if let Some(parent) = target.parent.as_ref() {
            if let Some(replaced) = replace_path_id(request.url(), "Shows", parent.member.as_str())
            {
                *request.url_mut() = replaced;
            }
        }

        let state = state.clone();
        join_set.spawn(async move {
            let result =
                fetch_paged_items_from_server(index, state, request, target.session, target.server)
                    .await;
            (
                index,
                result.map(|mut fetched| {
                    fetched.source_parent_id = source_parent_id;
                    fetched
                }),
            )
        });
    }

    if join_set.is_empty() && failures == 0 {
        return Ok(FetchedCatalog {
            server_items: Vec::new(),
            response_shape: ResponseShape::Counted,
            report: CatalogReport::new(outcomes, unavailable_targets),
        });
    }
    let (indexed_results, _) = collect_federated_results(
        join_set,
        failures,
        &mut outcomes,
        |items: &FetchedServerItems| items.outcome,
    )
    .await?;
    let server_items = indexed_results
        .into_iter()
        .map(|(_, items)| items)
        .collect::<Vec<_>>();
    let response_shape = ResponseShape::from_responses(
        server_items
            .iter()
            .map(|items| &items.server_items.response),
    );

    Ok(FetchedCatalog {
        server_items,
        response_shape,
        report: CatalogReport::new(outcomes, unavailable_targets),
    })
}

impl FetchedServerItems {
    fn bounded(server_items: ServerItems) -> Self {
        Self {
            outcome: FetchOutcome::BoundedFeed,
            ..Self::complete(server_items)
        }
    }
    fn complete(server_items: ServerItems) -> Self {
        Self {
            server_items,
            outcome: FetchOutcome::Complete,
            source_parent_id: None,
        }
    }
}

#[derive(Clone, Copy)]
enum ScanPolicy {
    Items,
    // Views has an unpaginated first request and a bare response is a complete
    // inventory. Continuations still use the same validation and deadline.
    LibraryInventory,
}

pub(crate) async fn fetch_catalog(
    state: &MediaServices,
    original_request: &reqwest::Request,
    policy: &CatalogRequestPolicy,
    targets: Vec<CatalogFetchTarget>,
    mode: FetchMode,
    failures: usize,
) -> Result<FetchedCatalog, ApplicationError> {
    fetch_catalog_operation(
        state,
        CatalogFetchInput::Http(original_request),
        policy,
        targets,
        mode,
        failures,
        fetch_server_items,
    )
    .await
}

/// Background callers supply catalog intent, not a synthetic incoming request.
/// Actual outgoing requests are constructed for each selected target here.
pub(super) async fn fetch_refresh_catalog(
    state: &MediaServices,
    query: &super::query::CatalogQuery,
    policy: &CatalogRequestPolicy,
    targets: Vec<CatalogFetchTarget>,
    failures: usize,
) -> Result<FetchedCatalog, ApplicationError> {
    fetch_catalog_operation(
        state,
        CatalogFetchInput::Inventory(query),
        policy,
        targets,
        FetchMode::VirtualLibrary,
        failures,
        fetch_server_items,
    )
    .await
}

enum CatalogFetchInput<'a> {
    Http(&'a reqwest::Request),
    Inventory(&'a super::query::CatalogQuery),
}

impl CatalogFetchInput<'_> {
    fn request_for(&self, server: &Server) -> Option<reqwest::Request> {
        match self {
            Self::Http(request) => request.try_clone(),
            Self::Inventory(query) => Some(reqwest::Request::new(
                reqwest::Method::GET,
                query.to_url(server.url.as_url().clone()),
            )),
        }
    }
}

struct SourceRequest {
    index: usize,
    state: MediaServices,
    request: reqwest::Request,
    target: CatalogFetchTarget,
    pagination: Pagination,
    upstream_limited: bool,
    library_inventory: bool,
}

async fn fetch_server_items(
    source: SourceRequest,
) -> Result<(FetchedServerItems, ResponseShape), ApplicationError> {
    let SourceRequest {
        index,
        state,
        request,
        target,
        pagination,
        upstream_limited,
        library_inventory,
    } = source;
    let result = if upstream_limited || library_inventory {
        let items = fetch_items_from_server(
            index,
            state,
            request,
            target.session,
            target.server,
            pagination,
        )
        .await?;
        if upstream_limited {
            FetchedServerItems::bounded(items)
        } else {
            FetchedServerItems::complete(items)
        }
    } else {
        fetch_paged_items_from_server(index, state, request, target.session, target.server).await?
    };
    let shape = ResponseShape::from_responses([&result.server_items.response]);
    Ok((result, shape))
}

pub(super) struct CompactSource {
    pub(super) listing: StoredListing,
    pub(super) keys: Vec<SortKey>,
}

pub(super) async fn fetch_compact_catalog(
    state: &MediaServices,
    request: &reqwest::Request,
    policy: &CatalogRequestPolicy,
    targets: Vec<CatalogFetchTarget>,
    failures: usize,
    name_policy: crate::media::presentation::ItemNamePolicy,
) -> Result<FetchedCatalog<CompactSource>, ApplicationError> {
    let sort = Arc::new(policy.sort.clone());
    fetch_catalog_operation(
        state,
        CatalogFetchInput::Http(request),
        policy,
        targets,
        FetchMode::Listing,
        failures,
        move |source| fetch_compact_source(source, sort.clone(), name_policy),
    )
    .await
}

async fn fetch_compact_source(
    source: SourceRequest,
    sort: Arc<SortPolicy>,
    name_policy: crate::media::presentation::ItemNamePolicy,
) -> Result<(FetchedServerItems<CompactSource>, ResponseShape), ApplicationError> {
    let SourceRequest {
        index,
        state,
        request,
        target,
        ..
    } = source;
    let mut listing = StoredListing::new(state.catalog_snapshots.item_store());
    let mut keys = Vec::new();
    let shape = scan_pages(
        index,
        &state,
        request,
        &target.session,
        &target.server,
        ScanPolicy::Items,
        |page| {
            let mut items = page.into_items();
            for item in &mut items {
                keys.push(sort.key(item));
                name_policy.apply(item, &target.server.name);
            }
            listing.extend_items(items)
        },
    )
    .await?;
    Ok((
        FetchedServerItems {
            server_items: CompactSource { listing, keys },
            outcome: FetchOutcome::Complete,
            source_parent_id: None,
        },
        shape,
    ))
}

async fn fetch_catalog_operation<T, F, Fut>(
    state: &MediaServices,
    input: CatalogFetchInput<'_>,
    policy: &CatalogRequestPolicy,
    targets: Vec<CatalogFetchTarget>,
    mode: FetchMode,
    mut failures: usize,
    fetch_source: F,
) -> Result<FetchedCatalog<T>, ApplicationError>
where
    T: Send + 'static,
    F: Fn(SourceRequest) -> Fut,
    Fut: Future<Output = Result<(FetchedServerItems<T>, ResponseShape), ApplicationError>>
        + Send
        + 'static,
{
    let unavailable_targets = failures;
    let mut join_set = JoinSet::new();
    let mut outcomes = source_outcomes(&targets);

    for (index, target) in targets.into_iter().enumerate() {
        let Some(mut request) = input.request_for(&target.server) else {
            error!("Failed to clone request for server: {}", target.server.name);
            failures += 1;
            continue;
        };
        if !super::filters::restrict_named_ids(state, request.url_mut(), target.server.id).await? {
            continue;
        }
        policy.prepare_upstream(
            request.url_mut(),
            target.parent.is_some() || matches!(mode, FetchMode::VirtualLibrary),
        );
        let source_parent_id = target.parent.as_ref().map(|parent| parent.backend.clone());
        if let Some(parent) = target.parent.as_ref() {
            *request.url_mut() =
                replace_aggregate_parent_id(request.url(), &parent.requested, &parent.member);
        }
        // A bounded row needs only each backend's first `Limit` items, in one
        // request that keeps the client's `EnableTotalRecordCount=false`.
        let upstream_limited = policy.endpoint().is_upstream_limited() || policy.is_bounded_row();
        let library_inventory = policy.endpoint().is_library_root();
        let pagination = policy.pagination;
        let fetch = fetch_source(SourceRequest {
            index,
            state: state.clone(),
            request,
            target,
            pagination,
            upstream_limited,
            library_inventory,
        });
        join_set.spawn(async move {
            let result = fetch.await.map(|(mut fetched, shape)| {
                fetched.source_parent_id = source_parent_id;
                (fetched, shape)
            });
            (index, result)
        });
    }

    if join_set.is_empty() && failures == 0 {
        return Ok(FetchedCatalog {
            server_items: Vec::new(),
            response_shape: ResponseShape::Counted,
            report: CatalogReport::new(outcomes, unavailable_targets),
        });
    }
    let (indexed_results, _) =
        collect_federated_results(join_set, failures, &mut outcomes, |(items, _)| {
            items.outcome
        })
        .await?;
    let mut response_shape = ResponseShape::Bare;
    let server_items = indexed_results
        .into_iter()
        .map(|(_, (items, shape))| {
            if shape == ResponseShape::Counted {
                response_shape = shape;
            }
            items
        })
        .collect();

    Ok(FetchedCatalog {
        server_items,
        response_shape,
        report: CatalogReport::new(outcomes, unavailable_targets),
    })
}

async fn collect_federated_results<T: Send + 'static>(
    mut join_set: JoinSet<(usize, Result<T, ApplicationError>)>,
    mut failures: usize,
    outcomes: &mut [SourceOutcome],
    outcome: impl Fn(&T) -> FetchOutcome,
) -> Result<(Vec<(usize, T)>, usize), ApplicationError> {
    let mut indexed_results = Vec::new();
    let mut causes = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((index, Ok(items))) => {
                let result = outcome(&items);
                outcomes[index].outcome = result;
                if matches!(result, FetchOutcome::Partial(_)) {
                    failures += 1;
                } else {
                    indexed_results.push((index, items));
                }
            }
            Ok((index, Err(e))) => {
                failures += 1;
                error!("Federated server request failed: {:?}", e);
                if let ApplicationError::IncompleteCatalog { reason, .. } = &e {
                    outcomes[index].outcome = FetchOutcome::Partial(*reason);
                }
                causes.push(e);
            }
            Err(e) => {
                failures += 1;
                error!("Task failed: {:?}", e);
                causes.push(ApplicationError::Internal(e.into()));
            }
        }
    }

    if indexed_results.is_empty() {
        error!("All federated server requests failed");
        return Err(ApplicationError::SourcesFailed(causes));
    }
    if failures > 0 {
        warn!(
            failures,
            "Serving available federation sources; failed sources retain their inventories"
        );
    }

    indexed_results.sort_by_key(|(index, _)| *index);
    Ok((indexed_results, failures))
}

pub(super) async fn fetch_inventory(
    state: &MediaServices,
    request: &reqwest::Request,
    targets: Vec<CatalogFetchTarget>,
) -> Result<FetchedCatalog<RawServerItems>, ApplicationError> {
    let mut tasks = JoinSet::new();
    let mut outcomes = source_outcomes(&targets);
    for (index, target) in targets.into_iter().enumerate() {
        let request = request
            .try_clone()
            .ok_or(ApplicationError::MissingIdentity(
                "cloneable catalog request",
            ))?;
        let state = state.clone();
        tasks.spawn(async move {
            (
                index,
                fetch_library_inventory(index, state, request, target.session, target.server).await,
            )
        });
    }
    let (results, _) = collect_federated_results(tasks, 0, &mut outcomes, |_: &RawServerItems| {
        FetchOutcome::Complete
    })
    .await?;
    let response_shape =
        ResponseShape::from_responses(results.iter().map(|(_, items)| &items.response));
    let server_items = results
        .into_iter()
        .map(|(_, server_items)| FetchedServerItems {
            server_items,
            outcome: FetchOutcome::Complete,
            source_parent_id: None,
        })
        .collect();
    Ok(FetchedCatalog {
        server_items,
        response_shape,
        report: CatalogReport::new(outcomes, 0),
    })
}

async fn fetch_items_from_server(
    index: usize,
    state: MediaServices,
    request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
    pagination: Pagination,
) -> Result<ServerItems, ApplicationError> {
    let proxy_api_key = JellyfinAuthorization::from_headers_url(request.headers(), request.url())
        .and_then(|auth| auth.token_ref().map(str::to_string));
    let RawServerItems {
        mut response,
        server,
    } = fetch_raw_items_from_server(index, state.clone(), request, session, server, pagination)
        .await?;

    process_items_response_json(&mut response, &state, &server, proxy_api_key.as_deref()).await?;

    debug!(
        "Successfully retrieved {} items from server: {}",
        response.len(),
        server.name
    );
    trace!(
        "Items from server '{}' at index {}: {}",
        server.name,
        index,
        serde_json::to_string(&response).unwrap_or_default()
    );

    Ok(ServerItems { response, server })
}

async fn fetch_paged_items_from_server(
    index: usize,
    state: MediaServices,
    request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
) -> Result<FetchedServerItems, ApplicationError> {
    let mut items = Vec::new();
    let shape = scan_pages(
        index,
        &state,
        request,
        &session,
        &server,
        ScanPolicy::Items,
        |page| {
            items.extend(page.into_items());
            Ok(())
        },
    )
    .await?;
    let count = items.len();
    Ok(FetchedServerItems::complete(ServerItems {
        response: shape.wrap(items, count, Pagination::unbounded()),
        server,
    }))
}

/// Pass each validated page to the consumer before requesting another batch.
/// Item scans translate and stage discoveries. A successful scan publishes
/// those discoveries, while inventory scans return raw IDs to their caller.
async fn scan_pages(
    index: usize,
    state: &MediaServices,
    request: reqwest::Request,
    session: &AuthorizationSession,
    server: &Server,
    policy: ScanPolicy,
    mut accept_page: impl FnMut(ItemsResponseVariants) -> Result<(), ApplicationError>,
) -> Result<ResponseShape, ApplicationError> {
    let proxy_api_key = JellyfinAuthorization::from_headers_url(request.headers(), request.url())
        .and_then(|auth| auth.token_ref().map(str::to_string));
    let request = prepare_backend_request(request, state, session, server).await?;
    let mut libraries = Vec::new();
    // The configured timeout bounds each page, not the size of a valid catalog.
    let page_timeout = Duration::from_secs(state.config.read().await.timeout.clamp(1, 300));
    let mut progress = ScanProgress::default();

    let mut known_total: Option<usize> = None;
    let mut page_size = UPSTREAM_PAGE_SIZE;
    'scan: loop {
        // Once a counted page has told us the size of the listing, fetch the
        // next few pages together. They are still accepted strictly in offset
        // order with the same validation as a sequential scan. Library
        // inventories and uncounted listings stay sequential.
        let offset = progress.offset();
        let batch = match (policy, known_total) {
            (ScanPolicy::Items, Some(total)) => total
                .saturating_sub(offset)
                .div_ceil(page_size)
                .clamp(1, UPSTREAM_SCAN_CONCURRENCY),
            _ => 1,
        };
        let mut pages = Vec::with_capacity(batch);
        for page in 0..batch {
            let mut page_request = request
                .try_clone()
                .ok_or(ApplicationError::MissingIdentity("cloneable catalog page"))?;
            if matches!(policy, ScanPolicy::Items) || offset > 0 {
                set_upstream_page(page_request.url_mut(), offset + page * page_size, page_size);
            }
            pages.push(tokio::time::timeout(
                page_timeout,
                execute_raw_items_request(index, state.clone(), page_request, server.clone()),
            ));
        }
        for page in futures_util::future::join_all(pages).await {
            let page = match page {
                Ok(Ok(page)) => page.response,
                Ok(Err(status)) if progress.offset() == 0 => return Err(status),
                Err(error) if progress.offset() == 0 => {
                    return Err(
                        ApplicationError::Internal(error.into()).at(FailureStage::CatalogDeadline)
                    )
                }
                Ok(Err(error)) => {
                    return Err(ApplicationError::IncompleteCatalog {
                        reason: PartialReason::Failed,
                        cause: Some(Box::new(error)),
                    })
                }
                Err(error) => {
                    return Err(ApplicationError::IncompleteCatalog {
                        reason: PartialReason::Deadline,
                        cause: Some(Box::new(ApplicationError::Internal(error.into()))),
                    })
                }
            };
            if let ItemsResponseVariants::WithCount(page) = &page {
                known_total = Some(page.total_record_count.max(0) as usize);
            }
            let bare_inventory = matches!(policy, ScanPolicy::LibraryInventory)
                && matches!(page, ItemsResponseVariants::Bare(_));
            let received = page.len();
            let step = progress.accept(&page)?;
            let page = if matches!(policy, ScanPolicy::Items) {
                libraries.extend(discovered_libraries_from_response(server.id, &page));
                translate_page(page, state, server, proxy_api_key.as_deref()).await?
            } else {
                page
            };
            accept_page(page)?;
            if step == super::scan::ScanStep::Complete || bare_inventory {
                break 'scan;
            }
            if received < page_size {
                // A backend may return a short page before the end. Later
                // pages of this batch then start at the wrong offset, so drop
                // them and continue from what was actually received.
                if known_total.is_some() && matches!(policy, ScanPolicy::Items) {
                    page_size = received;
                }
                continue 'scan;
            }
        }
    }

    if matches!(policy, ScanPolicy::Items) {
        track_libraries(state, server, &libraries).await;
    }
    Ok(if known_total.is_some() {
        ResponseShape::Counted
    } else {
        ResponseShape::Bare
    })
}

async fn fetch_raw_items_from_server(
    index: usize,
    state: MediaServices,
    mut request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
    pagination: Pagination,
) -> Result<RawServerItems, ApplicationError> {
    // Library discovery must cover the whole inventory, independently of the
    // client's display page. Some upstreams cap even an unbounded request.
    if CatalogEndpoint::from_url(request.url()).is_library_root() {
        return fetch_library_inventory(index, state, request, session, server).await;
    }
    normalize_upstream_pagination(request.url_mut(), pagination);
    let request = prepare_backend_request(request, &state, &session, &server).await?;
    let items = execute_raw_items_request(index, state.clone(), request, server).await?;
    track_discovered_libraries(&state, &items.server, &items.response).await;
    Ok(items)
}

async fn fetch_library_inventory(
    index: usize,
    state: MediaServices,
    mut request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
) -> Result<RawServerItems, ApplicationError> {
    // Keep the first request unpaginated for ordinary Jellyfin user Views.
    // MediaFolders can be an administrator-wide inventory; filtered Views are
    // not evidence that omitted libraries are inaccessible to this viewer.
    let viewer_inventory = CatalogEndpoint::from_url(request.url()) == CatalogEndpoint::UserViews
        && request.url().query_pairs().all(|(key, _)| {
            matches!(
                key.to_ascii_lowercase().as_str(),
                "userid"
                    | "fields"
                    | "startindex"
                    | "limit"
                    | "enableimages"
                    | "enableuserdata"
                    | "enableimagetypes"
                    | "imagetypelimit"
                    | "api_key"
                    | "apikey"
            )
        });
    let generation = if viewer_inventory {
        Some(
            state
                .virtual_library_service
                .begin_viewer_inventory()
                .await
                .map_err(ApplicationError::Storage)?,
        )
    } else {
        None
    };
    normalize_upstream_pagination(request.url_mut(), Pagination::unbounded());
    let mut items = Vec::new();
    let shape = scan_pages(
        index,
        &state,
        request,
        &session,
        &server,
        ScanPolicy::LibraryInventory,
        |page| {
            items.extend(page.into_items());
            Ok(())
        },
    )
    .await?;
    let count = items.len();
    let response = shape.wrap(items, count, Pagination::unbounded());
    // Commit discoveries only after all pages succeeded. A failed continuation
    // must not make a partial inventory look authoritative.
    track_discovered_libraries(&state, &server, &response).await;
    if let Some(generation) = generation {
        state
            .virtual_library_service
            .replace_viewer_libraries(
                &session.user_id,
                server.id,
                generation,
                &discovered_libraries_from_response(server.id, &response),
            )
            .await
            .map_err(ApplicationError::Storage)?;
    }
    Ok(RawServerItems { response, server })
}

async fn prepare_backend_request(
    request: reqwest::Request,
    state: &MediaServices,
    session: &AuthorizationSession,
    server: &Server,
) -> Result<reqwest::Request, ApplicationError> {
    let auth = JellyfinAuthorization::Authorization(session.to_authorization());
    // This executor only receives concrete targets from access-checked plans.
    // Preserve its established session Authorization wire style.
    let target = crate::application::request::context::AuthorizedTarget::new(
        server.clone(),
        Some(session.clone()),
        Some(auth),
        None,
    )
    .map_err(|error| ApplicationError::Internal(error.into()).at(FailureStage::CatalogEncoding))?;
    state
        .request_builder()
        .build(&target, &request)
        .await
        .map_err(Into::into)
}

async fn execute_raw_items_request(
    index: usize,
    state: MediaServices,
    request: reqwest::Request,
    server: Server,
) -> Result<RawServerItems, ApplicationError> {
    let response =
        crate::application::request::response::execute_json_request::<serde_json::Value>(
            &state.reqwest_client,
            request,
        )
        .await
        .inspect_err(|e| {
            error!("Failed to get items from server '{}': {:?}", server.name, e);
        })?;

    let items_response: ItemsResponseVariants = response_json_to_payload(response)?;
    debug!(
        "Fetched {} raw items from server '{}' at index {}",
        items_response.len(),
        server.name,
        index
    );

    Ok(RawServerItems {
        response: items_response,
        server,
    })
}

async fn track_discovered_libraries(
    state: &MediaServices,
    server: &Server,
    response: &ItemsResponseVariants,
) {
    let libraries = discovered_libraries_from_response(server.id, response);
    track_libraries(state, server, &libraries).await;
}

async fn track_libraries(state: &MediaServices, server: &Server, libraries: &[DiscoveredLibrary]) {
    if let Err(error) = state
        .virtual_library_service
        .track_discovered_libraries(libraries)
        .await
    {
        warn!(
            "Failed to track libraries observed on server '{}': {error}",
            server.name
        );
    }
}

fn discovered_libraries_from_response(
    server_id: ServerId,
    response: &ItemsResponseVariants,
) -> Vec<DiscoveredLibrary> {
    let items = match response {
        ItemsResponseVariants::WithCount(response) => &response.items,
        ItemsResponseVariants::Bare(items) => items,
    };
    items
        .iter()
        .filter_map(|item| {
            let collection_type = presentable_library_collection_type(item)?;
            let name = item.name.as_deref()?.trim();
            (!name.is_empty()).then(|| DiscoveredLibrary {
                server_id,
                original_library_id: item.id.clone(),
                name: name.to_string(),
                collection_type: collection_type.as_str().to_string(),
            })
        })
        .collect()
}

/// Translate a scanned listing one upstream page at a time. Each page goes
/// through the same JSON translation as before, in the same envelope shape, so
/// a large catalog never exists as a single JSON tree.
async fn process_items_response_json(
    response: &mut ItemsResponseVariants,
    state: &MediaServices,
    server: &Server,
    proxy_api_key: Option<&str>,
) -> Result<(), ApplicationError> {
    let (items, envelope) = match response {
        ItemsResponseVariants::WithCount(counted) => (
            std::mem::take(&mut counted.items),
            Some((counted.total_record_count, counted.start_index)),
        ),
        ItemsResponseVariants::Bare(items) => (std::mem::take(items), None),
    };
    let mut translated = Vec::with_capacity(items.len());
    let mut items = items.into_iter().peekable();
    // An empty listing still goes through translation once, as before.
    let mut first = true;
    while first || items.peek().is_some() {
        first = false;
        let chunk = items.by_ref().take(UPSTREAM_PAGE_SIZE).collect::<Vec<_>>();
        let page = match envelope {
            Some((total_record_count, start_index)) => {
                ItemsResponseVariants::WithCount(ItemsResponseWithCount {
                    items: chunk,
                    total_record_count,
                    start_index,
                })
            }
            None => ItemsResponseVariants::Bare(chunk),
        };
        let page = translate_page(page, state, server, proxy_api_key).await?;
        translated.extend(page.into_items());
    }
    match response {
        ItemsResponseVariants::WithCount(counted) => counted.items = translated,
        ItemsResponseVariants::Bare(items) => *items = translated,
    }
    Ok(())
}

async fn translate_page(
    page: ItemsResponseVariants,
    state: &MediaServices,
    server: &Server,
    proxy_api_key: Option<&str>,
) -> Result<ItemsResponseVariants, ApplicationError> {
    let mut page_json = serde_json::to_value(&page).map_err(ApplicationError::Serialization)?;
    drop(page);
    state
        .process_response_json(
            &mut page_json,
            server,
            ResponseProcessingProfile::Media,
            proxy_api_key,
        )
        .await
        .inspect_err(|error| {
            error!(
                "Failed to process media items from server '{}': {error:?}",
                server.name
            )
        })?;
    response_json_to_payload(page_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::models::MediaItem;
    use crate::test_support::MediaFixture;
    use hyper::StatusCode;
    use serde_json::json;

    async fn collision_fixture() -> (
        MediaFixture,
        CatalogFetchTarget,
        crate::media::storage::MediaMapping,
        wiremock::MockServer,
    ) {
        use crate::{
            config::{AppConfig, MediaStreamingMode},
            media::storage::MediaStorageService,
            persistence::MIGRATOR,
            protocol::device::Device,
            servers::storage::ServerStorageService,
        };
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let upstream = wiremock::MockServer::start().await;
        let servers = ServerStorageService::new(pool.clone());
        let id = servers
            .add_server(
                "Catalog owner",
                &upstream.uri(),
                100,
                MediaStreamingMode::Proxy,
            )
            .await
            .unwrap();
        let server = servers.get_server_by_id(id).await.unwrap().unwrap();
        let media = MediaStorageService::new(pool.clone());
        let decoy = media
            .get_or_create_media_mapping(&"wrong-parent".into(), &server)
            .await
            .unwrap();
        let parent = media
            .get_or_create_media_mapping(&decoy.virtual_media_id.as_str().into(), &server)
            .await
            .unwrap();
        let season_decoy = media
            .get_or_create_media_mapping(&"wrong-season".into(), &server)
            .await
            .unwrap();
        let season = media
            .get_or_create_media_mapping(&season_decoy.virtual_media_id.as_str().into(), &server)
            .await
            .unwrap();
        let state = MediaFixture::new(pool, AppConfig::default());
        let session = AuthorizationSession {
            id: 1,
            user_id: "viewer".into(),
            server_url: upstream.uri(),
            device: Device {
                client: "test".into(),
                device: "test".into(),
                device_id: "test".into(),
                version: "1".into(),
            },
            upstream_device_id: None,
            jellyfin_token: "backend-token".into(),
            original_user_id: "backend-viewer".into(),
        };
        (
            state,
            CatalogFetchTarget {
                session,
                server,
                parent: Some(super::super::library_resolution::ResolvedCatalogParent {
                    backend: parent.original_media_id,
                    member: parent.virtual_media_id,
                    requested: "aggregate".into(),
                }),
            },
            season,
            upstream,
        )
    }

    #[tokio::test]
    async fn catalog_fanout_preserves_json_causes_including_failed_continuation_pages() {
        use wiremock::{matchers::method, Mock, ResponseTemplate};
        for continuation in [false, true] {
            let (state, target, _, upstream) = collision_fixture().await;
            Mock::given(method("GET")).respond_with(move |request: &wiremock::Request| {
                let first_page = request.url.query_pairs().any(|(key, value)| key == "StartIndex" && value == "0");
                if continuation && first_page {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "Items":[{"Id":"first", "Type":"Movie"}], "TotalRecordCount":2, "StartIndex":0
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_string("private-invalid-payload")
                }
            }).mount(&upstream).await;
            let request = reqwest::Request::new(
                reqwest::Method::GET,
                "http://proxy/Items?ParentId=aggregate&Recursive=true"
                    .parse()
                    .unwrap(),
            );
            let policy = CatalogRequestPolicy::from_url(request.url());
            let error = match fetch_catalog(
                &state,
                &request,
                &policy,
                vec![target],
                FetchMode::VirtualLibrary,
                0,
            )
            .await
            {
                Ok(_) => panic!("invalid JSON must fail catalog scan"),
                Err(error) => error,
            };

            let ApplicationError::SourcesFailed(causes) = &error else {
                panic!("typed source failures required")
            };
            assert_eq!(causes.len(), 1);
            if continuation {
                assert!(matches!(
                    causes[0],
                    ApplicationError::IncompleteCatalog {
                        reason: PartialReason::Failed,
                        ..
                    }
                ));
            }
            let mut cause: &(dyn std::error::Error + 'static) = &error;
            while !cause.is::<serde_json::Error>() {
                cause = cause
                    .source()
                    .expect("original JSON cause must survive federation");
            }
            assert!(!format!("{error:?}").contains("private-invalid-payload"));
            assert_eq!(
                upstream.received_requests().await.unwrap().len(),
                if continuation { 2 } else { 1 }
            );
        }
    }

    #[tokio::test]
    async fn catalog_parent_collision_translates_member_once_and_retains_original_metadata() {
        use wiremock::{
            matchers::{method, path, query_param},
            Mock, ResponseTemplate,
        };
        for key in ["pArEnTiD", "sErIeSiD", "sEaSoNiD"] {
            let (state, target, _, upstream) = collision_fixture().await;
            let original_parent = target.parent.as_ref().unwrap().backend.as_str().to_owned();
            Mock::given(method("GET"))
                .and(path("/Items"))
                .and(query_param(key, &original_parent))
                .and(query_param("Extension", "opaque/value"))
                .and(query_param("StartIndex", "0"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"Items":[],"TotalRecordCount":0,"StartIndex":0})),
                )
                .expect(1)
                .mount(&upstream)
                .await;
            let original = reqwest::Request::new(reqwest::Method::GET,
                format!("http://proxy/Items?{key}=aggregate&Extension=opaque%2Fvalue&StartIndex=7&Limit=2").parse().unwrap());
            let policy = CatalogRequestPolicy::from_url(original.url());
            let fetched = fetch_catalog(
                &state,
                &original,
                &policy,
                vec![target],
                FetchMode::VirtualLibrary,
                0,
            )
            .await
            .unwrap();
            assert_eq!(
                fetched.report.outcomes()[0]
                    .parent_id
                    .as_ref()
                    .map(|id| id.as_str()),
                Some(original_parent.as_str())
            );
            assert_eq!(
                fetched.server_items[0]
                    .source_parent_id
                    .as_ref()
                    .map(|id| id.as_str()),
                Some(original_parent.as_str())
            );
            let traffic = upstream.received_requests().await.unwrap();
            assert_eq!(traffic.len(), 1);
            assert!(!traffic[0].url.as_str().contains("wrong-parent"));
            assert!(original.url().as_str().contains("aggregate"));
        }
    }

    #[tokio::test]
    async fn show_and_season_collisions_translate_members_once_and_retain_original_metadata() {
        use wiremock::{
            matchers::{method, path, query_param},
            Mock, ResponseTemplate,
        };
        for endpoint in ["Seasons", "Episodes"] {
            let (state, target, season, upstream) = collision_fixture().await;
            let original_parent = target.parent.as_ref().unwrap().backend.as_str().to_owned();
            Mock::given(method("GET"))
                .and(path(format!("/Shows/{original_parent}/{endpoint}")))
                .and(query_param("sEaSoNiD", season.original_media_id.as_str()))
                .and(query_param("Extension", "kept"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"Items":[],"TotalRecordCount":0,"StartIndex":0})),
                )
                .expect(1)
                .mount(&upstream)
                .await;
            let original = reqwest::Request::new(
                reqwest::Method::GET,
                format!(
                    "http://proxy/Shows/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/{endpoint}?sEaSoNiD={}&Extension=kept",
                    season.virtual_media_id.as_str()
                )
                .parse()
                .unwrap(),
            );
            let policy = CatalogRequestPolicy::from_url(original.url());
            let fetched = fetch_show_catalog(&state, &original, &policy, vec![target], 0)
                .await
                .unwrap();
            assert_eq!(
                fetched.report.outcomes()[0]
                    .parent_id
                    .as_ref()
                    .map(|id| id.as_str()),
                Some(original_parent.as_str())
            );
            assert_eq!(
                fetched.server_items[0]
                    .source_parent_id
                    .as_ref()
                    .map(|id| id.as_str()),
                Some(original_parent.as_str())
            );
            let traffic = upstream.received_requests().await.unwrap();
            assert_eq!(traffic.len(), 1);
            assert!(!traffic[0].url.as_str().contains("wrong-parent"));
            assert!(!traffic[0].url.as_str().contains("wrong-season"));
        }
    }

    #[tokio::test]
    async fn collection_retains_scan_reason_while_discarding_partial_items() {
        let mut tasks = JoinSet::new();
        tasks.spawn(async { (0, Ok(FetchOutcome::Complete)) });
        tasks.spawn(async { (1, Ok(FetchOutcome::Partial(PartialReason::Changed))) });
        tasks.spawn(async {
            (
                2,
                Err(ApplicationError::UpstreamStatus(StatusCode::BAD_GATEWAY)),
            )
        });
        let mut outcomes = (0..3)
            .map(|index| SourceOutcome {
                server_id: ServerId::new(index),
                parent_id: Some(format!("library-{index}").into()),
                outcome: FetchOutcome::Partial(PartialReason::Failed),
            })
            .collect::<Vec<_>>();
        let (visible, failures) =
            collect_federated_results(tasks, 0, &mut outcomes, |outcome| *outcome)
                .await
                .unwrap();
        assert_eq!(visible, vec![(0, FetchOutcome::Complete)]);
        assert_eq!(failures, 2);
        assert_eq!(
            outcomes[1].outcome,
            FetchOutcome::Partial(PartialReason::Changed)
        );
        assert_eq!(
            outcomes[1].parent_id.as_ref().map(|id| id.as_str()),
            Some("library-1")
        );
        assert_eq!(
            outcomes[2].outcome,
            FetchOutcome::Partial(PartialReason::Failed)
        );
    }

    #[test]
    fn raw_library_responses_expose_real_libraries_for_tracking() {
        let mut library = typed_media_item("real-library-id", "CollectionFolder", Some("movies"));
        library.name = Some("Movies".to_string());
        let mut live_tv = typed_media_item("live-tv", "UserView", Some("livetv"));
        live_tv.name = Some("Live TV".to_string());
        let response = ItemsResponseVariants::Bare(vec![
            library,
            live_tv,
            typed_media_item("movie", "Movie", None),
        ]);

        assert_eq!(
            discovered_libraries_from_response(ServerId::new(7), &response),
            vec![DiscoveredLibrary {
                server_id: ServerId::new(7),
                original_library_id: "real-library-id".to_string(),
                name: "Movies".to_string(),
                collection_type: "movies".to_string(),
            }]
        );
    }

    fn typed_media_item(id: &str, item_type: &str, collection_type: Option<&str>) -> MediaItem {
        let mut item = json!({
            "Id": id,
            "Type": item_type,
        });

        if let Some(collection_type) = collection_type {
            item["CollectionType"] = json!(collection_type);
        }

        serde_json::from_value(item).unwrap()
    }
}
