//! Proxy-side compatibility for Jellyfin Web's single-episode play queue.
//! Ordinary StartItemId requests retain their inclusive, forward-only semantics.
use super::{
    fetch::fetch_show_catalog,
    finalize_items_response,
    library_resolution::CatalogFetchTarget,
    postprocessing::{FederatedItems, ResponseShape},
    request_policy::CatalogRequestPolicy,
};
use crate::{
    application::{
        details::get_item_detail,
        error::ApplicationError,
        media_services::MediaServices,
        request::{context::Principal, input::OperationRequest},
    },
    media::{
        presentation::ItemNamePolicy,
        scope::{MediaCatalogScope, MediaScopeKind},
    },
};
use serde_json::Value;

const QUEUE_LIMIT: usize = 100;

pub(super) struct WebEpisodeQueue {
    anchor: String,
    viewer: String,
    scope: String,
}

impl WebEpisodeQueue {
    pub(super) fn from_request(input: &OperationRequest) -> Option<Self> {
        if input.context.principal() != Principal::ProxyUser
            || !input
                .context
                .device()?
                .client
                .eq_ignore_ascii_case("Jellyfin Web")
        {
            return None;
        }
        let url = input.original_request.url();
        let policy = CatalogRequestPolicy::from_url(url);
        let anchor = policy.episode_start_id?;
        if policy.pagination.start_index != 0 || policy.pagination.limit != Some(QUEUE_LIMIT) {
            return None;
        }
        let pairs = url.query_pairs().collect::<Vec<_>>();
        let is_false = |name: &str| {
            pairs.iter().any(|(key, value)| {
                key.eq_ignore_ascii_case(name) && value.eq_ignore_ascii_case("false")
            })
        };
        let has_field = |field: &str| {
            pairs
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case("Fields"))
                .any(|(_, fields)| {
                    fields
                        .split(',')
                        .any(|value| value.trim().eq_ignore_ascii_case(field))
                })
        };
        // Recognize the stock web playback request, not season browsing,
        // explicitly sorted queries, or another client's anchored pagination.
        if !is_false("IsMissing")
            || !is_false("IsVirtualUnaired")
            || !["Chapters", "MediaSources", "Trickplay"]
                .into_iter()
                .all(has_field)
            || pairs.iter().any(|(key, _)| {
                ["SeasonId", "SortBy", "SortOrder"]
                    .iter()
                    .any(|name| key.eq_ignore_ascii_case(name))
            })
        {
            return None;
        }
        let series = crate::protocol::url::contains_id(url, "Shows")?;
        let viewer = input.context.access_scope()?.user_id().to_string();
        let scope = MediaCatalogScope {
            kind: MediaScopeKind::Aggregate,
            viewer: &viewer,
            resource_id: &series,
        }
        .to_string();
        Some(Self {
            anchor,
            viewer,
            scope,
        })
    }

    pub(super) fn prepare(&self, url: &mut url::Url) {
        let pairs = url
            .query_pairs()
            .filter(|(key, _)| {
                !["StartItemId", "StartIndex", "Limit"]
                    .iter()
                    .any(|name| key.eq_ignore_ascii_case(name))
            })
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }

    pub(super) async fn apply(
        &self,
        state: &MediaServices,
        mut response: Value,
    ) -> Result<Value, ApplicationError> {
        let items = response
            .get_mut("Items")
            .and_then(Value::as_array_mut)
            .ok_or(ApplicationError::InvalidUpstreamPayload(
                "episode queue Items must be an array",
            ))?;
        let exact = items
            .iter()
            .position(|item| item["Id"].as_str() == Some(&self.anchor));
        let mut selected = exact;
        if selected.is_none() {
            let mut members = vec![self.anchor.clone()];
            members.extend(
                state
                    .media_storage
                    .get_media_version_members_by_virtual_id(&self.anchor)
                    .await
                    .map_err(ApplicationError::Storage)?
                    .into_iter()
                    .map(|member| member.mapping.virtual_media_id),
            );
            if let Some((_, owner)) = state
                .media_storage
                .get_media_version_detail_route(&self.anchor, &self.viewer)
                .await
                .map_err(ApplicationError::Storage)?
            {
                members.push(owner.virtual_media_id);
            }
            for member in members {
                if let Some(group) = state
                    .media_storage
                    .get_media_parent_group_id_for_scope(&member, &self.viewer, Some(&self.scope))
                    .await
                    .map_err(ApplicationError::Storage)?
                {
                    selected = items
                        .iter()
                        .position(|item| item["Id"].as_str() == Some(&group));
                    if selected.is_some() {
                        break;
                    }
                }
            }
        }
        let total = items.len();
        let start = match (exact, selected) {
            (Some(index), _) => index
                .saturating_sub(QUEUE_LIMIT / 2)
                .min(total.saturating_sub(QUEUE_LIMIT)),
            // Web falls back to index zero when an alternate's Id is absent.
            // Keep its primary first so it cannot play an earlier episode with
            // the selected alternate's MediaSourceId.
            (None, Some(index)) => index,
            (None, None) => total,
        };
        let window = items
            .drain(start..(start + QUEUE_LIMIT).min(total))
            .collect();
        *items = window;
        response["StartIndex"] = serde_json::json!(if exact.is_some() { start } else { 0 });
        response["TotalRecordCount"] = serde_json::json!(if exact.is_some() {
            total
        } else {
            total - start
        });
        Ok(response)
    }
}

/// Queue compatibility needs a complete ordered list even when the series lives
/// only on one backend. Reuse the catalog scanner's capped-page and failure rules
/// rather than sending an unbounded request or querying unrelated servers.
pub(super) async fn single_show_children(
    state: &MediaServices,
    input: OperationRequest,
    scan_queue: bool,
) -> Result<Value, ApplicationError> {
    if !scan_queue {
        return get_item_detail(state, input, false).await;
    }
    let target = input.resolve_target(state).await?.target;
    let server = target.server().clone();
    let session = input
        .context
        .backends()
        .sessions
        .as_ref()
        .and_then(|sessions| {
            sessions
                .iter()
                .find(|(_, candidate)| candidate.id == server.id)
        })
        .map(|(session, _)| session.clone())
        .ok_or(ApplicationError::UnavailableOwner)?;
    let policy = CatalogRequestPolicy::from_url(input.original_request.url());
    let fetched = fetch_show_catalog(
        state,
        &input.original_request,
        &policy,
        vec![CatalogFetchTarget {
            session,
            server,
            parent: None,
        }],
        0,
    )
    .await?;
    let items = FederatedItems::from_servers(
        fetched
            .server_items
            .into_iter()
            .map(|source| source.server_items)
            .collect(),
        ItemNamePolicy::Preserve,
    );
    finalize_items_response(
        state,
        &input.original_request,
        items,
        &policy,
        ResponseShape::Counted,
    )
    .await
}
