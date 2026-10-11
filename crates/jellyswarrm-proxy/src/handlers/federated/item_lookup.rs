//! `Ids` is a lookup of client-facing identities, not a new deduplication scope.
//! In particular, playback queues must not expand an aggregate into its copies.
use std::collections::{HashMap, HashSet};

use axum::Json;
use hyper::StatusCode;

use crate::{
    media_catalog::{merge_media_group, TaggedMediaItem},
    media_storage_service::{MediaMapping, MediaStorageService},
    request_preprocessing::PreprocessedRequest,
    AppState,
};

use super::{
    finalize_items_response,
    library_resolution::CatalogFetchTarget,
    postprocessing::{FederatedItems, ResponseShape, SortPolicy},
    request_policy::CatalogRequestPolicy,
    upstream::{fetch_catalog, FetchMode},
};

struct RequestedItem {
    id: String,
    aggregate: bool,
    members: Vec<MediaMapping>,
}

pub(super) struct RequestedItems(Vec<RequestedItem>);

impl RequestedItems {
    pub async fn resolve(
        state: &AppState,
        request: &PreprocessedRequest,
    ) -> Result<Option<Self>, StatusCode> {
        let url = request.original_request.url();
        if !url
            .path()
            .trim_end_matches('/')
            .to_ascii_lowercase()
            .ends_with("/items")
        {
            return Ok(None);
        }
        let ids = url
            .query_pairs()
            .filter(|(key, _)| key.eq_ignore_ascii_case("Ids"))
            .flat_map(|(_, value)| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(MediaStorageService::normalize_uuid)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        for id in ids.into_iter().filter(|id| seen.insert(id.clone())) {
            let group = state
                .media_storage
                .get_media_version_group(&id)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let aggregate = group.is_some();
            let members = if let Some(group) = group {
                state
                    .media_storage
                    .get_media_version_members(group.id)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                    .into_iter()
                    .map(|member| member.mapping)
                    .collect::<Vec<_>>()
            } else {
                state
                    .media_storage
                    .get_media_mapping_by_virtual(&id)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                    .into_iter()
                    .collect()
            };
            let members = members
                .into_iter()
                .filter(|member| {
                    request
                        .access_scope
                        .as_ref()
                        .is_some_and(|scope| scope.allows(member.server_id))
                })
                .collect();
            entries.push(RequestedItem {
                id,
                aggregate,
                members,
            });
        }
        Ok(entries
            .iter()
            .any(|entry| entry.aggregate)
            .then_some(Self(entries)))
    }

    pub async fn fetch(
        self,
        state: &AppState,
        request: PreprocessedRequest,
    ) -> Result<Json<serde_json::Value>, StatusCode> {
        let mut policy = CatalogRequestPolicy::from_url(request.original_request.url());
        if !request
            .original_request
            .url()
            .query_pairs()
            .any(|(key, _)| key.eq_ignore_ascii_case("SortBy"))
        {
            policy.sort = SortPolicy::PreserveUpstream;
        }
        let sessions = request.sessions.as_ref().ok_or(StatusCode::UNAUTHORIZED)?;
        let mut seen_servers = HashSet::new();
        let mut fetches = Vec::new();
        for (session, server) in sessions
            .iter()
            .filter(|(_, server)| seen_servers.insert(server.id))
        {
            let ids = self
                .0
                .iter()
                .flat_map(|entry| &entry.members)
                .filter(|member| member.server_id == server.id)
                .map(|member| member.virtual_media_id.clone())
                .collect::<HashSet<_>>();
            if ids.is_empty() {
                continue;
            }
            let mut ids = ids.into_iter().collect::<Vec<_>>();
            ids.sort();
            let mut upstream_request = request
                .original_request
                .try_clone()
                .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
            let pairs = upstream_request
                .url()
                .query_pairs()
                .filter(|(key, _)| !key.eq_ignore_ascii_case("Ids"))
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>();
            upstream_request
                .url_mut()
                .query_pairs_mut()
                .clear()
                .extend_pairs(pairs)
                .append_pair("Ids", &ids.join(","));
            let target = CatalogFetchTarget {
                session: session.clone(),
                server: server.clone(),
                parent_id: None,
                resolved_parent_id: None,
            };
            let policy = &policy;
            fetches.push(async move {
                fetch_catalog(
                    state,
                    &upstream_request,
                    policy,
                    vec![target],
                    FetchMode::Listing,
                    0,
                )
                .await
            });
        }
        let had_targets = !fetches.is_empty();
        if !had_targets && self.0.iter().any(|entry| !entry.members.is_empty()) {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let mut successful = false;
        let mut fetched = HashMap::new();
        for result in futures_util::future::join_all(fetches).await {
            let Ok(catalog) = result else {
                continue;
            };
            successful = true;
            for source in catalog.server_items {
                let server = source.server_items.server;
                for item in source.server_items.response.into_items() {
                    fetched.insert(
                        item.id.clone(),
                        TaggedMediaItem {
                            item,
                            server: server.clone(),
                        },
                    );
                }
            }
        }
        if had_targets && !successful {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let mut items: Vec<crate::media_presentation::CatalogItem> = Vec::new();
        for entry in self.0 {
            let members = entry
                .members
                .iter()
                .filter_map(|member| fetched.get(&member.virtual_media_id).cloned())
                .collect::<Vec<_>>();
            if members.is_empty() {
                continue;
            }
            if entry.aggregate {
                items.push(merge_media_group(members, &entry.id).into());
            } else {
                items.extend(members.into_iter().map(|member| member.item.into()));
            }
        }
        if let Some(scope) = &request.access_scope {
            for item in &mut items {
                crate::handlers::media_versions::preserve_media_parent_groups(
                    state,
                    item,
                    scope.user_id(),
                )
                .await?;
            }
        }
        let response = finalize_items_response(
            state,
            &request.original_request,
            FederatedItems::from_merged_items(items),
            &policy,
            ResponseShape::Counted,
        )
        .await?;
        if let Some(token) = request.auth.as_ref().and_then(|auth| auth.token_ref()) {
            state
                .client_sessions
                .cache_media_response(token, &response.0)
                .await;
        }
        Ok(response)
    }
}
