//! Explicit background inventory refresh. No scheduler or incoming HTTP request.
use super::services::CatalogService;
use crate::{
    application::error::ApplicationError,
    media_storage_service::ReconciliationMode,
    user_authorization_service::AuthorizationSession,
    virtual_library_service::{VirtualLibraryAccessScope, VirtualLibraryResolution},
};

use super::{
    library_resolution::{CatalogFetchTarget, ResolvedCatalogParent},
    media_reconciliation::{reconcile_fetched_catalog, ReconciliationContext},
    query::CatalogQuery,
    scan::CatalogReport,
    upstream::fetch_refresh_catalog,
};

/// Caller-provided credentials and authorization are explicit. A background
/// caller resolves them through the normal session service, never an invented
/// HTTP request. Only exhaustive virtual-library inventories are supported.
pub(crate) struct CatalogRefresh {
    pub library_id: String,
    pub access_scope: VirtualLibraryAccessScope,
    pub sessions: Vec<AuthorizationSession>,
}

// Read by future refresh callers; no scheduler is enabled by this extraction.
#[allow(dead_code)]
pub(crate) struct RefreshResult {
    pub report: CatalogReport,
    pub response: serde_json::Value,
}

/// Reserved service entry point for on-demand/background refresh callers.
/// Scheduling is intentionally not enabled by this API.
#[allow(dead_code)]
pub(crate) async fn refresh(
    state: &CatalogService,
    input: CatalogRefresh,
) -> Result<RefreshResult, ApplicationError> {
    if input
        .sessions
        .iter()
        .any(|session| session.user_id != input.access_scope.user_id())
    {
        return Err(ApplicationError::AccessDenied);
    }
    let resolved = state
        .virtual_library_service
        .resolve(&input.library_id, Some(&input.access_scope))
        .await
        .map_err(ApplicationError::Storage)?;
    let resolved = match resolved {
        VirtualLibraryResolution::Resolved(resolved) => resolved,
        // No source inventory was read. In particular, do not treat an empty
        // resolution as an authoritative deletion of any previous inventory.
        VirtualLibraryResolution::Empty { .. } => {
            return Ok(RefreshResult {
                report: CatalogReport::new(Vec::new(), 1),
                response: serde_json::json!({"Items": [], "TotalRecordCount": 0, "StartIndex": 0}),
            })
        }
        VirtualLibraryResolution::Unknown => {
            return Err(ApplicationError::InvalidInput("unknown refresh library"))
        }
    };
    let mut targets = Vec::new();
    let mut skipped = 0;
    for member in resolved.members {
        if !input.access_scope.allows(member.server.id) {
            return Err(ApplicationError::AccessDenied);
        }
        let Some(session) = input
            .sessions
            .iter()
            .find(|session| session.server_url == member.server.url.as_str())
        else {
            skipped += 1;
            continue;
        };
        targets.push(CatalogFetchTarget {
            session: session.clone(),
            server: member.server,
            parent: Some(ResolvedCatalogParent {
                backend: member.mapping.original_media_id.into(),
                member: member.mapping.virtual_media_id.into(),
                requested: input.library_id.clone().into(),
            }),
        });
    }
    if targets.is_empty() {
        return Err(ApplicationError::UnavailableOwner);
    }
    let query = CatalogQuery::inventory(&input.library_id);
    let mut policy = query.policy(true);
    policy.reconcile_playback = state.deduplicate_media_enabled().await;
    // Fence before fetching, exactly as in the interactive workflow.
    let generation = if policy.reconcile_playback {
        ReconciliationMode::Enabled {
            generation: state
                .media_storage
                .begin_media_reconciliation()
                .await
                .map_err(ApplicationError::Storage)?,
        }
    } else {
        ReconciliationMode::Disabled
    };
    let fetched = fetch_refresh_catalog(state, &query, &policy, targets, skipped).await?;
    let report = fetched.report.clone();
    let shape = fetched.response_shape;
    let items = reconcile_fetched_catalog(
        state,
        ReconciliationContext {
            viewer: input.access_scope.user_id(),
            catalog_scope_key: &resolved.catalog_scope_key,
            series_group: None,
            generation,
        },
        &policy,
        fetched,
    )
    .await?;
    let response = serde_json::to_value(items.into_response_with_policy(&policy, shape))
        .map_err(ApplicationError::Serialization)?;
    // Successful refreshes fence interactive snapshots, including in-flight
    // builds. Refresh itself neither caches a client page nor observes a client.
    state
        .catalog_snapshots
        .invalidate_viewer(input.access_scope.user_id());
    Ok(RefreshResult { report, response })
}
