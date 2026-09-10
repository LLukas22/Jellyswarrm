use std::collections::HashMap;

use axum::Json;
use hyper::StatusCode;
use tokio::task::JoinSet;
use tracing::{error, warn};

use crate::{
    handlers::common::response_json_to_payload,
    models::{ItemsResponseVariants, MediaItem},
    processors::response_processor::ResponseProcessingProfile,
    request_preprocessing::PreprocessedRequest,
    server_id::ServerId,
    server_storage::Server,
    virtual_library_service::{
        compare_virtual_library_routes, normalize_library_id, VirtualLibraryAccessScope,
        VirtualLibraryResolution,
    },
    AppState,
};

use super::{
    finalize_items_response,
    item_policy::{
        automatic_library_key, is_live_tv_user_view, presentable_library_collection_type,
    },
    library_resolution::CatalogFetchTarget,
    postprocessing::{FederatedItems, ServerItems},
    upstream::{fetch_catalog, FetchMode, FetchedCatalog, FetchedServerItems},
};

struct AutomaticGroupPresentation {
    items: Vec<MediaItem>,
    discovered_members: Vec<(String, ServerId, String)>,
}

struct BuiltVirtualLibrary {
    item: MediaItem,
    members: Vec<(ServerId, String)>,
}

struct NamedMediaItemGroup {
    name: String,
    sort_order: i32,
    items: Vec<ServerMediaItem>,
}

struct LibraryRootInventory {
    configured_groups: HashMap<String, NamedMediaItemGroup>,
    unassigned_libraries: Vec<ServerMediaItem>,
    non_library_per_server: Vec<ServerItems>,
}

#[derive(Clone)]
struct ServerMediaItem {
    item: MediaItem,
    server: Server,
}

async fn process_library_group_individually(
    state: &AppState,
    group: Vec<ServerMediaItem>,
) -> Result<Vec<MediaItem>, StatusCode> {
    let mut items = Vec::with_capacity(group.len());
    for ServerMediaItem { item, server } in group {
        items.push(process_media_item_for_server(item, state, &server, true).await?);
    }
    Ok(items)
}

async fn present_automatic_library_group(
    state: &AppState,
    key: String,
    group: Vec<ServerMediaItem>,
    access_scope: &VirtualLibraryAccessScope,
    complete_refresh: bool,
) -> Result<AutomaticGroupPresentation, StatusCode> {
    if group.len() == 1 {
        if !complete_refresh {
            if let Some(automatic) = state
                .virtual_library_service
                .get_automatic_library_by_collection_type(&key)
                .await
                .map_err(|error| {
                    error!("Failed to load automatic library: {error}");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
            {
                let has_accessible_members = matches!(
                    state
                        .virtual_library_service
                        .resolve(&automatic.virtual_id, Some(access_scope))
                        .await
                        .map_err(|error| {
                            error!("Failed to resolve automatic library snapshot: {error}");
                            StatusCode::INTERNAL_SERVER_ERROR
                        })?,
                    VirtualLibraryResolution::Resolved(_)
                );
                if has_accessible_members {
                    let display_name = group
                        .first()
                        .and_then(|source| source.item.name.clone())
                        .unwrap_or_else(|| key.clone());
                    let built = build_virtual_library_item(
                        state,
                        group,
                        display_name,
                        automatic.virtual_id.clone(),
                    )
                    .await?;
                    let discovered_members = built
                        .members
                        .into_iter()
                        .map(|(server_id, member_id)| {
                            (automatic.virtual_id.clone(), server_id, member_id)
                        })
                        .collect();
                    return Ok(AutomaticGroupPresentation {
                        items: vec![built.item],
                        discovered_members,
                    });
                }
            }
        }

        return Ok(AutomaticGroupPresentation {
            items: process_library_group_individually(state, group).await?,
            discovered_members: Vec::new(),
        });
    }

    let display_name = group
        .first()
        .and_then(|source| source.item.name.clone())
        .unwrap_or_else(|| {
            key.split_once(':')
                .map(|(_, name)| name.to_string())
                .unwrap_or_else(|| key.clone())
        });
    let automatic = match state
        .virtual_library_service
        .get_or_create_automatic_library(&key, &display_name)
        .await
    {
        Ok(automatic) => automatic,
        Err(error) => {
            error!("Failed to get/create merged library for '{key}': {error}");
            return Ok(AutomaticGroupPresentation {
                items: process_library_group_individually(state, group).await?,
                discovered_members: Vec::new(),
            });
        }
    };
    let built =
        build_virtual_library_item(state, group, display_name, automatic.virtual_id.clone())
            .await?;
    let discovered_members = built
        .members
        .into_iter()
        .map(|(server_id, member_id)| (automatic.virtual_id.clone(), server_id, member_id))
        .collect();
    Ok(AutomaticGroupPresentation {
        items: vec![built.item],
        discovered_members,
    })
}

async fn partition_library_root_inventory(
    state: &AppState,
    server_items: Vec<FetchedServerItems>,
) -> Result<LibraryRootInventory, StatusCode> {
    let assignments = state
        .virtual_library_service
        .get_assignments()
        .await
        .map_err(|error| {
            error!("Failed to load configured library assignments: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let mut configured_groups: HashMap<String, NamedMediaItemGroup> = HashMap::new();
    let mut unassigned_libraries = Vec::new();
    let mut non_library_per_server = Vec::new();
    let mut live_tv_seen = false;

    for fetched in server_items {
        let ServerItems { response, server } = fetched.server_items;
        let mut non_library_items = Vec::new();

        for item in response.into_items() {
            if let Some(collection_type) = presentable_library_collection_type(&item) {
                let original_library_id = normalize_library_id(&item.id);
                if let Some(assignment) = assignments.get(&(server.id, original_library_id)) {
                    if assignment
                        .collection_type
                        .as_deref()
                        .is_some_and(|group_type| {
                            group_type.eq_ignore_ascii_case(collection_type.as_str())
                        })
                    {
                        configured_groups
                            .entry(assignment.group_virtual_id.clone())
                            .or_insert_with(|| NamedMediaItemGroup {
                                name: assignment.group_name.clone(),
                                sort_order: assignment.group_sort_order,
                                items: Vec::new(),
                            })
                            .items
                            .push(ServerMediaItem {
                                item,
                                server: server.clone(),
                            });
                    } else {
                        warn!(
                            "Ignoring type-incompatible library assignment for '{}' on server {}",
                            item.name.as_deref().unwrap_or(&item.id),
                            server.id
                        );
                        unassigned_libraries.push(ServerMediaItem {
                            item,
                            server: server.clone(),
                        });
                    }
                } else {
                    unassigned_libraries.push(ServerMediaItem {
                        item,
                        server: server.clone(),
                    });
                }
                continue;
            }

            if is_live_tv_user_view(&item) {
                if live_tv_seen {
                    continue;
                }
                live_tv_seen = true;
            }
            non_library_items.push(item);
        }

        if !non_library_items.is_empty() {
            let processed =
                process_media_items_for_server(non_library_items, state, &server, true).await?;
            if !processed.is_empty() {
                non_library_per_server.push(ServerItems {
                    response: ItemsResponseVariants::Bare(processed),
                    server,
                });
            }
        }
    }

    Ok(LibraryRootInventory {
        configured_groups,
        unassigned_libraries,
        non_library_per_server,
    })
}

pub(super) async fn get_automatic_library_root(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    targets: Vec<CatalogFetchTarget>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let PreprocessedRequest {
        original_request,
        access_scope,
        ..
    } = preprocessed;
    let access_scope = access_scope.ok_or(StatusCode::UNAUTHORIZED)?;
    let refresh_generation = state
        .virtual_library_service
        .begin_automatic_library_refresh(&access_scope)
        .await;

    let FetchedCatalog {
        server_items,
        failures,
        response_shape,
    } = fetch_catalog(state, &original_request, targets, FetchMode::Inventory, 0).await?;
    let refreshed_server_ids = server_items
        .iter()
        .map(|items| items.server_items.server.id)
        .collect::<Vec<_>>();
    let LibraryRootInventory {
        configured_groups,
        unassigned_libraries,
        non_library_per_server,
    } = partition_library_root_inventory(state, server_items).await?;
    let mut library_groups: HashMap<String, Vec<ServerMediaItem>> = HashMap::new();
    for source in unassigned_libraries {
        let Some(key) = automatic_library_key(&source.item) else {
            continue;
        };
        library_groups.entry(key).or_default().push(source);
    }

    let mut library_items = present_configured_library_groups(state, configured_groups).await?;
    let mut discovered_members = Vec::new();
    let mut automatic_groups = library_groups.into_iter().collect::<Vec<_>>();
    automatic_groups.sort_by(|left, right| left.0.cmp(&right.0));
    for (key, group) in automatic_groups {
        let presentation =
            present_automatic_library_group(state, key, group, &access_scope, failures == 0)
                .await?;
        library_items.extend(presentation.items);
        discovered_members.extend(presentation.discovered_members);
    }

    state
        .virtual_library_service
        .reconcile_automatic_library_members(
            &access_scope,
            refresh_generation,
            &refreshed_server_ids,
            &discovered_members,
        )
        .await
        .map_err(|e| {
            error!("Failed to reconcile automatic library members: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let items = FederatedItems::new(library_items).merge_interleaved(non_library_per_server);
    finalize_items_response(items, original_request.url(), response_shape)
}

async fn present_configured_library_groups(
    state: &AppState,
    configured_library_groups: HashMap<String, NamedMediaItemGroup>,
) -> Result<Vec<MediaItem>, StatusCode> {
    let mut group_entries = configured_library_groups.into_iter().collect::<Vec<_>>();
    group_entries.sort_by(|left, right| {
        left.1
            .sort_order
            .cmp(&right.1.sort_order)
            .then_with(|| left.1.name.cmp(&right.1.name))
            .then_with(|| left.0.cmp(&right.0))
    });

    let mut library_join = JoinSet::new();
    for (index, (group_virtual_id, group)) in group_entries.into_iter().enumerate() {
        let state = state.clone();
        library_join.spawn(async move {
            let item =
                build_virtual_library_item(&state, group.items, group.name, group_virtual_id)
                    .await
                    .map(|built| built.item);
            (index, item)
        });
    }

    let mut indexed_library_items = Vec::new();
    while let Some(result) = library_join.join_next().await {
        match result {
            Ok((index, Ok(item))) => indexed_library_items.push((index, item)),
            Ok((_, Err(status))) => return Err(status),
            Err(error) => {
                error!("Library folder processing failed: {error:?}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }
    indexed_library_items.sort_by_key(|(index, _)| *index);
    Ok(indexed_library_items
        .into_iter()
        .map(|(_, item)| item)
        .collect())
}

pub(super) async fn get_configured_library_root(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    targets: Vec<CatalogFetchTarget>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = preprocessed.original_request;
    let FetchedCatalog {
        server_items,
        response_shape,
        ..
    } = fetch_catalog(state, &original_request, targets, FetchMode::Inventory, 0).await?;
    let LibraryRootInventory {
        configured_groups,
        unassigned_libraries,
        non_library_per_server,
    } = partition_library_root_inventory(state, server_items).await?;
    let mut library_items = present_configured_library_groups(state, configured_groups).await?;
    let mut single_groups = HashMap::new();
    for source in unassigned_libraries {
        let key = format!(
            "single:{}:{}",
            source.server.id,
            normalize_library_id(&source.item.id)
        );
        single_groups.entry(key).or_insert(source);
    }
    let mut single_groups = single_groups.into_iter().collect::<Vec<_>>();
    single_groups.sort_by(|left, right| left.0.cmp(&right.0));
    for (_key, ServerMediaItem { item, server }) in single_groups {
        library_items.push(process_library_folder(state, item, &server, true).await?);
    }

    let items = FederatedItems::new(library_items).merge_interleaved(non_library_per_server);

    finalize_items_response(items, original_request.url(), response_shape)
}

async fn process_media_items_for_server(
    items: Vec<MediaItem>,
    state: &AppState,
    server: &Server,
    should_change_name: bool,
) -> Result<Vec<MediaItem>, StatusCode> {
    let mut processed = Vec::with_capacity(items.len());
    for item in items {
        processed
            .push(process_media_item_for_server(item, state, server, should_change_name).await?);
    }
    Ok(processed)
}

async fn build_virtual_library_item(
    state: &AppState,
    group: Vec<ServerMediaItem>,
    display_name: String,
    virtual_id: String,
) -> Result<BuiltVirtualLibrary, StatusCode> {
    let mut members = Vec::new();
    let mut template = None;
    let mut total_child_count = 0;

    let preferred_source_index =
        preferred_library_source_index(&group).ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let image_source_index =
        preferred_library_image_source_index(&group).unwrap_or(preferred_source_index);
    let primary_tag = group[image_source_index]
        .item
        .image_tags
        .as_ref()
        .and_then(|tags| tags.get("Primary").cloned());
    let mut image_source_id = None;

    for (index, ServerMediaItem { item, server }) in group.into_iter().enumerate() {
        total_child_count += item.child_count.unwrap_or(0);
        let processed = process_media_item_for_server(item, state, &server, false).await?;
        members.push((server.id, processed.id.clone()));
        if index == image_source_index {
            image_source_id = Some(processed.id.clone());
        }
        if index == preferred_source_index {
            template = Some(processed);
        }
    }

    let mut item = template.ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    item.id = virtual_id.clone();
    item.display_preferences_id = Some(virtual_id);
    item.name = Some(display_name.clone());
    item.sort_name = Some(display_name.to_lowercase());
    item.child_count = Some(total_child_count);
    // Metadata stays on the preferred source. Artwork falls back to the next-best
    // server that actually has a Primary image; the concrete member ID also
    // prevents image requests from being re-routed through the merged snapshot.
    if let Some(image_source_id) = image_source_id {
        attach_library_folder_image_source(
            &mut item,
            &image_source_id,
            primary_tag.as_deref(),
            false,
        );
    }
    Ok(BuiltVirtualLibrary { item, members })
}

fn preferred_library_source_index(group: &[ServerMediaItem]) -> Option<usize> {
    group
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| {
            compare_virtual_library_routes(
                &left.server,
                &left.item.id,
                &right.server,
                &right.item.id,
            )
        })
        .map(|(index, _)| index)
}

/// Picks the highest-ranked server that actually has a Primary image, walking down
/// the same deterministic order used for request routing.
fn preferred_library_image_source_index(group: &[ServerMediaItem]) -> Option<usize> {
    group
        .iter()
        .enumerate()
        .filter(|(_, source)| {
            source
                .item
                .image_tags
                .as_ref()
                .is_some_and(|tags| tags.contains_key("Primary"))
        })
        .max_by(|(_, left), (_, right)| {
            compare_virtual_library_routes(
                &left.server,
                &left.item.id,
                &right.server,
                &right.item.id,
            )
        })
        .map(|(index, _)| index)
}

async fn process_library_folder(
    state: &AppState,
    item: MediaItem,
    server: &Server,
    should_change_name: bool,
) -> Result<MediaItem, StatusCode> {
    let primary_tag = item
        .image_tags
        .as_ref()
        .and_then(|tags| tags.get("Primary").cloned());
    let mut processed =
        process_media_item_for_server(item, state, server, should_change_name).await?;
    let image_source_id = processed.id.clone();
    attach_library_folder_image_source(
        &mut processed,
        &image_source_id,
        primary_tag.as_deref(),
        true,
    );
    Ok(processed)
}

fn attach_library_folder_image_source(
    item: &mut MediaItem,
    image_source_id: &str,
    primary_tag: Option<&str>,
    use_item_id_for_image: bool,
) {
    let Some(primary_tag) = primary_tag.map(str::to_string).or_else(|| {
        item.image_tags
            .as_ref()
            .and_then(|tags| tags.get("Primary").cloned())
    }) else {
        return;
    };

    if let Some(image_tags) = item.image_tags.as_mut() {
        image_tags.remove("Primary");
        if image_tags.is_empty() {
            item.image_tags = None;
        }
    }

    item.extra.insert(
        "PrimaryImageItemId".to_string(),
        serde_json::Value::String(image_source_id.to_string()),
    );
    item.extra.insert(
        "PrimaryImageTag".to_string(),
        serde_json::Value::String(primary_tag.clone()),
    );
    if use_item_id_for_image {
        item.image_tags
            .get_or_insert_with(HashMap::new)
            .insert("Primary".to_string(), primary_tag);
    }
}

async fn process_media_item_for_server(
    item: MediaItem,
    state: &AppState,
    server: &Server,
    should_change_name: bool,
) -> Result<MediaItem, StatusCode> {
    let mut item_json = serde_json::to_value(item).map_err(|e| {
        error!("Failed to serialize media item JSON: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    state
        .process_response_json(
            &mut item_json,
            server,
            ResponseProcessingProfile::Media,
            should_change_name,
            None,
        )
        .await?;

    response_json_to_payload(item_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::MediaStreamingMode, server_url::ServerUrl};
    use serde_json::json;

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

    fn library_source(
        id: &str,
        image_tag: Option<&str>,
        server_id: i64,
        priority: i32,
    ) -> ServerMediaItem {
        let mut item = typed_media_item(id, "CollectionFolder", Some("movies"));
        if let Some(image_tag) = image_tag {
            item.image_tags = Some(HashMap::from([(
                "Primary".to_string(),
                image_tag.to_string(),
            )]));
        }
        ServerMediaItem {
            item,
            server: Server {
                id: ServerId::new(server_id),
                name: format!("Server {server_id}"),
                url: ServerUrl::parse(&format!("http://server-{server_id}.example")).unwrap(),
                priority,
                media_streaming_mode: MediaStreamingMode::Redirect,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        }
    }

    #[test]
    fn attach_library_folder_image_source_points_client_at_source_library() {
        let mut item = typed_media_item("group-id", "CollectionFolder", Some("movies"));
        item.image_tags = Some(std::collections::HashMap::from([(
            "Primary".to_string(),
            "tag-123".to_string(),
        )]));

        super::attach_library_folder_image_source(
            &mut item,
            "source-library-id",
            Some("tag-123"),
            true,
        );

        assert_eq!(
            item.image_tags
                .as_ref()
                .and_then(|tags| tags.get("Primary"))
                .map(String::as_str),
            Some("tag-123")
        );
        assert_eq!(
            item.extra.get("PrimaryImageItemId"),
            Some(&json!("source-library-id"))
        );
        assert_eq!(item.extra.get("PrimaryImageTag"), Some(&json!("tag-123")));
    }

    #[test]
    fn merged_library_image_uses_same_server_priority_as_request_routing() {
        let group = vec![
            library_source("fast-response", Some("wrong-tag"), 1, 100),
            library_source("preferred", Some("routable-tag"), 2, 200),
        ];

        let selected = preferred_library_source_index(&group).unwrap();

        assert_eq!(group[selected].item.id, "preferred");
        assert_eq!(
            group[selected]
                .item
                .image_tags
                .as_ref()
                .and_then(|tags| tags.get("Primary"))
                .map(String::as_str),
            Some("routable-tag")
        );
    }

    #[test]
    fn merged_library_source_is_stable_when_response_order_changes() {
        let group = vec![
            library_source("lower-priority", Some("wrong-tag"), 1, 100),
            library_source("preferred", Some("preferred-tag"), 2, 200),
        ];
        let reversed = vec![
            library_source("preferred", Some("preferred-tag"), 2, 200),
            library_source("lower-priority", Some("wrong-tag"), 1, 100),
        ];

        let selected = preferred_library_source_index(&group).unwrap();
        let reversed_selected = preferred_library_source_index(&reversed).unwrap();

        assert_eq!(group[selected].item.id, "preferred");
        assert_eq!(reversed[reversed_selected].item.id, "preferred");
    }

    #[test]
    fn merged_library_artwork_falls_back_to_next_ranked_source_with_image() {
        let group = vec![
            library_source("with-image", Some("fallback-tag"), 1, 100),
            library_source("preferred-without-image", None, 2, 200),
        ];

        let metadata = preferred_library_source_index(&group).unwrap();
        let artwork = preferred_library_image_source_index(&group).unwrap();

        assert_eq!(group[metadata].item.id, "preferred-without-image");
        assert_eq!(group[artwork].item.id, "with-image");
    }

    #[test]
    fn merged_library_artwork_selection_is_stable_when_response_order_changes() {
        let group = vec![
            library_source("lowest", Some("lowest-tag"), 1, 100),
            library_source("middle", Some("middle-tag"), 2, 200),
            library_source("preferred-no-image", None, 3, 300),
        ];
        let reversed: Vec<_> = group.clone().into_iter().rev().collect();

        let selected = preferred_library_image_source_index(&group).unwrap();
        let reversed_selected = preferred_library_image_source_index(&reversed).unwrap();

        assert_eq!(group[selected].item.id, "middle");
        assert_eq!(reversed[reversed_selected].item.id, "middle");
    }

    #[test]
    fn library_image_omits_direct_tag_when_item_id_routes_to_another_source() {
        let mut item = typed_media_item("group-id", "CollectionFolder", Some("movies"));

        super::attach_library_folder_image_source(
            &mut item,
            "fallback-source-id",
            Some("fallback-tag"),
            false,
        );

        assert_eq!(
            (
                item.image_tags
                    .as_ref()
                    .is_some_and(|tags| tags.contains_key("Primary")),
                item.extra.get("PrimaryImageItemId"),
                item.extra.get("PrimaryImageTag"),
            ),
            (
                false,
                Some(&json!("fallback-source-id")),
                Some(&json!("fallback-tag")),
            )
        );
    }
}
