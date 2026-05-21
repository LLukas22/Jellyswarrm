use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use regex::Regex;
use std::sync::LazyLock;
use tokio::task::JoinSet;
use tracing::{debug, error, trace};

use crate::{
    handlers::{
        common::{execute_json_request, process_media_item},
        items::get_items,
    },
    merged_library_service::MergedLibraryMember,
    models::{
        enums::{BaseItemKind, CollectionType},
        MediaItem,
    },
    request_preprocessing::{apply_to_request, extract_request_infos, JellyfinAuthorization},
    AppState,
};

static SERIES_OR_PARENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("(?i)(seriesid|parentid)").unwrap());

fn extract_parent_id(query: &str) -> Option<String> {
    url::Url::parse(&format!("http://x?{}", query))
        .ok()?
        .query_pairs()
        .find(|(k, _)| k.eq_ignore_ascii_case("parentid"))
        .map(|(_, v)| v.into_owned())
}

fn extract_pagination(url: &url::Url) -> (usize, usize) {
    let mut start = 0usize;
    let mut limit = usize::MAX;
    for (k, v) in url.query_pairs() {
        if k.eq_ignore_ascii_case("startIndex") {
            start = v.parse().unwrap_or(0);
        } else if k.eq_ignore_ascii_case("limit") {
            limit = v.parse().unwrap_or(usize::MAX);
        }
    }
    (start, limit)
}

// Strips startIndex/limit before fan-out — each server must return all its items so
// we can merge and re-paginate the combined result ourselves.
fn strip_pagination(url: &url::Url) -> url::Url {
    let new_query: String = url
        .query_pairs()
        .filter(|(k, _)| {
            !k.eq_ignore_ascii_case("startIndex") && !k.eq_ignore_ascii_case("limit")
        })
        .map(|(k, v)| {
            format!(
                "{}={}",
                k,
                percent_encoding::utf8_percent_encode(&v, percent_encoding::NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    let mut new_url = url.clone();
    new_url.set_query(if new_query.is_empty() {
        None
    } else {
        Some(&new_query)
    });
    new_url
}

fn replace_parent_id(url: &url::Url, new_id: &str) -> url::Url {
    let new_query: String = url
        .query_pairs()
        .map(|(k, v)| {
            let val = if k.eq_ignore_ascii_case("parentid") {
                new_id.to_string()
            } else {
                v.into_owned()
            };
            format!(
                "{}={}",
                k,
                percent_encoding::utf8_percent_encode(&val, percent_encoding::NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&");

    let mut new_url = url.clone();
    new_url.set_query(if new_query.is_empty() {
        None
    } else {
        Some(&new_query)
    });
    new_url
}

pub async fn get_items_from_all_servers_if_not_restricted(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    if let Some(query) = req.uri().query() {
        if state.merge_libraries_enabled().await {
            if let Some(parent_id) = extract_parent_id(query) {
                match state.merged_library_service.resolve(&parent_id).await {
                    Ok(Some((lib, members))) if !members.is_empty() => {
                        debug!(
                            "ParentId {} is merged library '{}' — fanning out to {} servers",
                            parent_id,
                            lib.collection_type,
                            members.len()
                        );
                        return get_items_for_merged_library(State(state), req, members).await;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!("Failed to resolve merged library for {}: {}", parent_id, e);
                    }
                }
            }
        }

        if SERIES_OR_PARENT_RE.is_match(query) {
            return get_items(State(state), req).await;
        }
    }

    get_items_from_all_servers(State(state), req).await
}

async fn get_items_for_merged_library(
    State(state): State<AppState>,
    req: Request,
    members: Vec<MergedLibraryMember>,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    let (original_request, _, _, sessions, _) =
        extract_request_infos(req, &state).await.map_err(|e| {
            error!("Failed to preprocess merged-library request: {}", e);
            StatusCode::BAD_REQUEST
        })?;

    let sessions = sessions.ok_or(StatusCode::UNAUTHORIZED)?;
    if sessions.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Capture pagination from the original request; we strip it from per-server
    // requests so each server returns all its items, then paginate the merged result.
    let (start_index, limit) = extract_pagination(original_request.url());

    let mut join_set: JoinSet<(usize, Option<crate::models::ItemsResponseVariants>)> =
        JoinSet::new();

    for (index, member) in members.into_iter().enumerate() {
        let resolved = state
            .media_storage
            .get_media_mapping_with_server(&member.virtual_library_id)
            .await
            .map_err(|e| {
                error!(
                    "Failed to resolve member library {}: {}",
                    member.virtual_library_id, e
                );
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        let (mapping, server) = match resolved {
            Some(v) => v,
            None => {
                error!(
                    "No media mapping found for virtual library {}",
                    member.virtual_library_id
                );
                continue;
            }
        };

        let session = sessions
            .iter()
            .find(|(_, s)| {
                s.url.as_str().trim_end_matches('/') == server.url.as_str().trim_end_matches('/')
            })
            .map(|(sess, _)| sess.clone());

        let session = match session {
            Some(s) => s,
            None => {
                error!(
                    "No active session for server '{}' — skipping",
                    server.name
                );
                continue;
            }
        };

        let mut request = match original_request.try_clone() {
            Some(r) => r,
            None => {
                error!("Failed to clone request for merged library fan-out");
                continue;
            }
        };

        let new_url =
            strip_pagination(&replace_parent_id(request.url(), &mapping.original_media_id));
        *request.url_mut() = new_url;

        let auth = JellyfinAuthorization::Authorization(session.to_authorization());
        let state_clone = state.clone();
        let server_clone = server.clone();
        let session_clone = session.clone();

        join_set.spawn(async move {
            apply_to_request(
                &mut request,
                &server_clone,
                &Some(session_clone),
                &Some(auth),
                &state_clone,
            )
            .await;

            match execute_json_request::<crate::models::ItemsResponseVariants>(
                &state_clone.reqwest_client,
                request,
            )
            .await
            {
                Ok(mut resp) => {
                    let server_id = state_clone.config.read().await.server_id.clone();
                    for item in resp.iter_mut_items() {
                        match process_media_item(
                            item.clone(),
                            &state_clone,
                            &server_clone,
                            false,
                            &server_id,
                        )
                        .await
                        {
                            Ok(p) => *item = p,
                            Err(e) => {
                                error!("Failed to process item in merged fan-out: {:?}", e);
                                return (index, None);
                            }
                        }
                    }
                    debug!(
                        "Fan-out got {} items from '{}'",
                        resp.len(),
                        server_clone.name
                    );
                    (index, Some(resp))
                }
                Err(e) => {
                    error!(
                        "Merged library fan-out failed for '{}': {:?}",
                        server_clone.name, e
                    );
                    (index, None)
                }
            }
        });
    }

    let mut indexed: Vec<(usize, Option<crate::models::ItemsResponseVariants>)> = Vec::new();
    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(v) => indexed.push(v),
            Err(e) => error!("Task join error in merged fan-out: {:?}", e),
        }
    }
    indexed.sort_by_key(|(i, _)| *i);

    let server_responses: Vec<crate::models::ItemsResponseVariants> =
        indexed.into_iter().filter_map(|(_, v)| v).collect();
    let mut all_items: Vec<crate::models::MediaItem> = server_responses
        .into_iter()
        .flat_map(|r| r.into_items())
        .collect();
    all_items.sort_by(|a, b| {
        let ak = a.sort_name.as_deref().or(a.name.as_deref()).unwrap_or("");
        let bk = b.sort_name.as_deref().or(b.name.as_deref()).unwrap_or("");
        ak.cmp(bk)
    });
    let total = all_items.len() as i32;
    let paged: Vec<crate::models::MediaItem> =
        all_items.into_iter().skip(start_index).take(limit).collect();
    Ok(Json(crate::models::ItemsResponseVariants::WithCount(
        crate::models::ItemsResponseWithCount {
            items: paged,
            total_record_count: total,
            start_index: start_index as i32,
        },
    )))
}

pub async fn get_items_from_all_servers(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    let (original_request, _, _, sessions, _) =
        extract_request_infos(req, &state).await.map_err(|e| {
            error!("Failed to preprocess request: {}", e);
            StatusCode::BAD_REQUEST
        })?;

    let sessions = sessions.ok_or(StatusCode::UNAUTHORIZED)?;
    if sessions.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let mut join_set: JoinSet<(
        usize,
        Option<(crate::models::ItemsResponseVariants, crate::server_storage::Server)>,
    )> = JoinSet::new();

    for (index, (session, server)) in sessions.into_iter().enumerate() {
        let mut request = match original_request.try_clone() {
            Some(r) => r,
            None => {
                error!("Failed to clone request for server: {}", server.name);
                continue;
            }
        };

        let auth = JellyfinAuthorization::Authorization(session.to_authorization());
        let state_clone = state.clone();
        let server_clone = server.clone();
        let session_clone = session.clone();

        join_set.spawn(async move {
            apply_to_request(
                &mut request,
                &server_clone,
                &Some(session_clone),
                &Some(auth),
                &state_clone,
            )
            .await;

            match execute_json_request::<crate::models::ItemsResponseVariants>(
                &state_clone.reqwest_client,
                request,
            )
            .await
            {
                Ok(resp) => {
                    debug!(
                        "Fetched {} raw items from '{}'",
                        resp.len(),
                        server_clone.name
                    );
                    trace!(
                        "Raw items from '{}': {}",
                        server_clone.name,
                        serde_json::to_string(&resp).unwrap_or_default()
                    );
                    (index, Some((resp, server_clone)))
                }
                Err(e) => {
                    error!("Failed to fetch items from '{}': {:?}", server_clone.name, e);
                    (index, None)
                }
            }
        });
    }

    let mut indexed_raw = Vec::new();
    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(v) => indexed_raw.push(v),
            Err(e) => error!("Task failed: {:?}", e),
        }
    }
    indexed_raw.sort_by_key(|(i, _)| *i);

    let server_raw: Vec<(crate::models::ItemsResponseVariants, crate::server_storage::Server)> =
        indexed_raw.into_iter().filter_map(|(_, v)| v).collect();

    let cfg = state.config.read().await.clone();
    let server_id = cfg.server_id.clone();
    let merge_libraries = cfg.merge_libraries;
    drop(cfg);

    let mut library_groups: std::collections::HashMap<
        String,
        Vec<(MediaItem, crate::server_storage::Server)>,
    > = std::collections::HashMap::new();
    let mut non_lib_per_server: Vec<crate::models::ItemsResponseVariants> = Vec::new();

    // Deduplicate LiveTv across servers — keep at most one entry.
    let mut live_tv_seen = false;
    for (raw_response, server) in server_raw {
        let mut non_lib: Vec<MediaItem> = Vec::new();

        for item in raw_response.into_items() {
            let is_mergeable = merge_libraries
                && matches!(
                    item.item_type,
                    BaseItemKind::UserView | BaseItemKind::CollectionFolder
                )
                && item
                    .collection_type
                    .as_ref()
                    .map(|ct| *ct != CollectionType::LiveTv)
                    .unwrap_or(false);

            if is_mergeable {
                // Group by (collection_type, normalized_name) so "Movies" on server A only
                // merges with "Movies" on server B, not an unrelated "Spectacles" folder.
                let ct_str = serde_json::to_string(item.collection_type.as_ref().unwrap())
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string();
                let name_lower = item.name.as_deref().unwrap_or("").to_lowercase();
                let ct_key = format!("{}:{}", ct_str, name_lower);
                library_groups
                    .entry(ct_key)
                    .or_default()
                    .push((item, server.clone()));
            } else {
                if let Some(ct) = &item.collection_type {
                    if *ct == CollectionType::LiveTv && item.item_type == BaseItemKind::UserView {
                        if live_tv_seen {
                            continue;
                        }
                        live_tv_seen = true;
                    }
                }
                non_lib.push(item);
            }
        }

        if !non_lib.is_empty() {
            let mut processed: Vec<MediaItem> = Vec::new();
            for item in non_lib {
                match process_media_item(item, &state, &server, true, &server_id).await {
                    Ok(p) => processed.push(p),
                    Err(e) => error!(
                        "Failed to process non-library item from '{}': {:?}",
                        server.name, e
                    ),
                }
            }
            if !processed.is_empty() {
                non_lib_per_server.push(crate::models::ItemsResponseVariants::Bare(processed));
            }
        }
    }

    let mut library_items: Vec<MediaItem> = Vec::new();

    for (ct_key, group) in library_groups {
        if group.len() == 1 {
            let (item, server) = group.into_iter().next().unwrap();
            match process_media_item(item, &state, &server, true, &server_id).await {
                Ok(p) => library_items.push(p),
                Err(e) => error!("Failed to process single-server library: {:?}", e),
            }
        } else {
            let display_name = group[0]
                .0
                .name
                .clone()
                .unwrap_or_else(|| ct_key.splitn(2, ':').nth(1).map(str::to_string).unwrap_or_else(|| ct_key.clone()));

            let merged = match state
                .merged_library_service
                .get_or_create(&ct_key, &display_name)
                .await
            {
                Ok(m) => m,
                Err(e) => {
                    error!(
                        "Failed to get/create merged library for '{}': {}",
                        ct_key, e
                    );
                        for (item, server) in group {
                        if let Ok(p) =
                            process_media_item(item, &state, &server, true, &server_id).await
                        {
                            library_items.push(p);
                        }
                    }
                    continue;
                }
            };

            let mut members: Vec<(String, String)> = Vec::new();
            let mut template: Option<MediaItem> = None;
            let mut total_child_count: i32 = 0;

            for (item, server) in &group {
                total_child_count += item.child_count.unwrap_or(0);
                match process_media_item(
                    item.clone(),
                    &state,
                    server,
                    false, // no "[Server]" suffix — merged folder has a clean name
                    &server_id,
                )
                .await
                {
                    Ok(processed) => {
                        members.push((server.url.to_string(), processed.id.clone()));
                        if template.is_none() {
                            template = Some(processed);
                        }
                    }
                    Err(e) => error!(
                        "Failed to process library item for '{}': {:?}",
                        server.name, e
                    ),
                }
            }

            if let Err(e) = state
                .merged_library_service
                .upsert_members(&merged.virtual_id, &members)
                .await
            {
                error!("Failed to upsert merged library members: {}", e);
            }

            if let Some(mut tmpl) = template {
                tmpl.id = merged.virtual_id.clone();
                tmpl.name = Some(display_name);
                tmpl.child_count = Some(total_child_count);
                library_items.push(tmpl);
            }
        }
    }

    library_items.sort_by(|a, b| {
        let ak = a.name.as_deref().unwrap_or("");
        let bk = b.name.as_deref().unwrap_or("");
        ak.cmp(bk)
    });
    let mut final_items = library_items;
    final_items.extend(interleave(non_lib_per_server));

    let count = final_items.len();
    debug!("Returning {} items total", count);

    Ok(Json(crate::models::ItemsResponseVariants::WithCount(
        crate::models::ItemsResponseWithCount {
            items: final_items,
            total_record_count: count as i32,
            start_index: 0,
        },
    )))
}

fn interleave(server_items: Vec<crate::models::ItemsResponseVariants>) -> Vec<MediaItem> {
    let mut out = Vec::new();
    let max = server_items.iter().map(|s| s.len()).max().unwrap_or(0);
    for i in 0..max {
        for list in &server_items {
            if let Some(item) = list.get(i) {
                out.push(item.clone());
            }
        }
    }
    out
}
