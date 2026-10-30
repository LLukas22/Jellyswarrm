//! Catalog target planning and library eligibility/grouping policy.
use crate::models::{
    enums::{BaseItemKind, CollectionType},
    MediaItem,
};
use std::collections::HashSet;

use crate::application::error::{ApplicationError, FailureStage};
use crate::application::identity::{BackendItemId, VirtualItemId};
use tracing::{debug, error};

use super::services::CatalogService;
use crate::{
    application::input::OperationRequest,
    auth::service::AuthorizationSession,
    libraries::service::{normalize_library_id, LibraryGrouping, VirtualLibraryResolution},
    media::scope::{MediaCatalogScope, MediaScopeKind},
    servers::storage::Server,
};

pub(crate) enum CatalogPlan {
    EmptyVirtual,
    SingleServer,
    Virtual {
        catalog_scope_key: String,
        targets: Vec<CatalogFetchTarget>,
        skipped_targets: usize,
    },
    Interleaved(Vec<CatalogFetchTarget>),
    AutomaticRoot(Vec<CatalogFetchTarget>),
    ConfiguredRoot(Vec<CatalogFetchTarget>),
}

pub(crate) struct CatalogFetchTarget {
    pub(crate) session: AuthorizationSession,
    pub(crate) server: Server,
    pub(crate) parent: Option<ResolvedCatalogParent>,
}

/// Substitution and inventory attribution are one selection, never independently
/// optional. Only virtual identities enter the request builder.
#[derive(Debug)]
pub(crate) struct ResolvedCatalogParent {
    pub(crate) requested: VirtualItemId,
    pub(crate) member: VirtualItemId,
    pub(crate) backend: BackendItemId,
}

pub(super) fn unavailable_source_count(
    preprocessed: &OperationRequest,
    targets: &[CatalogFetchTarget],
) -> usize {
    preprocessed.context.access_scope().map_or(0, |scope| {
        scope
            .server_ids()
            .iter()
            .filter(|id| !targets.iter().any(|target| target.server.id == **id))
            .count()
    })
}

pub(crate) async fn resolve_catalog_plan(
    state: &CatalogService,
    preprocessed: &OperationRequest,
) -> Result<CatalogPlan, ApplicationError> {
    if let Some(parent_id) = parent_id(preprocessed.original_request.url()) {
        let resolution = state
            .virtual_library_service
            .resolve(&parent_id, preprocessed.context.access_scope())
            .await
            .map_err(ApplicationError::Storage)?;

        match resolution {
            VirtualLibraryResolution::Resolved(resolved) => {
                let sessions = available_sessions(preprocessed)?;
                let mut skipped_targets = 0;
                let member_count = resolved.members.len();
                let targets = resolved
                    .members
                    .into_iter()
                    .filter_map(|member| {
                        let session = sessions
                            .iter()
                            .find(|(_, server)| server.id == member.server.id)
                            .map(|(session, _)| session.clone());
                        let Some(session) = session else {
                            error!(
                                "No active session for server '{}' - skipping",
                                member.server.name
                            );
                            skipped_targets += 1;
                            return None;
                        };

                        Some(CatalogFetchTarget {
                            session,
                            server: member.server,
                            parent: Some(ResolvedCatalogParent {
                                backend: member.mapping.original_media_id.into(),
                                member: member.mapping.virtual_media_id.into(),
                                requested: parent_id.clone().into(),
                            }),
                        })
                    })
                    .collect();

                debug!(
                    "ParentId {} is virtual library '{}' - fanning out to {} members",
                    parent_id, resolved.name, member_count
                );
                return Ok(CatalogPlan::Virtual {
                    catalog_scope_key: resolved.catalog_scope_key,
                    targets,
                    skipped_targets,
                });
            }
            VirtualLibraryResolution::Empty { name } => {
                debug!("Virtual library '{name}' has no resolvable members");
                return Ok(CatalogPlan::EmptyVirtual);
            }
            VirtualLibraryResolution::Unknown => {
                if let Some(plan) = resolve_aggregate_plan(state, preprocessed, &parent_id).await? {
                    return Ok(plan);
                }
                if is_single_server_parent(state, &parent_id).await? {
                    return Ok(CatalogPlan::SingleServer);
                }
            }
        }
    }

    // Series/season scoped queries (e.g. Episodes?SeriesId=<aggregate>) fan out
    // the same way as ParentId so aggregate shows merge their children.
    if parent_id(preprocessed.original_request.url()).is_none() {
        if let Some(series_parent_id) = series_parent_id(preprocessed.original_request.url()) {
            if let Some(plan) =
                resolve_aggregate_plan(state, preprocessed, &series_parent_id).await?
            {
                return Ok(plan);
            }
        }
    }

    let grouping = if is_library_root_request(
        preprocessed.original_request.url(),
        state.get_url_prefix().await.as_deref(),
    ) {
        state
            .virtual_library_service
            .library_grouping(state.merge_libraries_enabled().await)
            .await
            .map_err(ApplicationError::Storage)?
    } else {
        LibraryGrouping::None
    };

    let targets: Vec<_> = available_sessions(preprocessed)?
        .into_iter()
        .map(|(session, server)| CatalogFetchTarget {
            session,
            server,
            parent: None,
        })
        .collect();

    let is_search = preprocessed
        .original_request
        .url()
        .query_pairs()
        .any(|(key, _)| key.eq_ignore_ascii_case("SearchTerm"));
    let is_resume = preprocessed
        .original_request
        .url()
        .path()
        .trim_end_matches('/')
        .to_ascii_lowercase()
        .ends_with("/resume");
    if (is_search
        || is_resume
        || preprocessed
            .original_request
            .url()
            .path()
            .to_ascii_lowercase()
            .ends_with("/latest"))
        && state.deduplicate_media_enabled().await
    {
        let viewer = preprocessed
            .context
            .access_scope()
            .map(|scope| scope.user_id())
            .or_else(|| preprocessed.context.user().map(|user| user.id.as_str()))
            .ok_or_else(|| {
                ApplicationError::MissingIdentity("catalog viewer")
                    .at(FailureStage::CallerAuthentication)
            })?;
        // Latest and search are additive feeds, not inventories. Keep their
        // identity scopes stable across windows, terms and missing sessions.
        return Ok(CatalogPlan::Virtual {
            catalog_scope_key: MediaCatalogScope {
                kind: if is_search {
                    MediaScopeKind::Search
                } else if is_resume {
                    MediaScopeKind::Resume
                } else {
                    MediaScopeKind::Latest
                },
                viewer,
                resource_id: &parent_id(preprocessed.original_request.url())
                    .map(|id| normalize_library_id(&id))
                    .unwrap_or_default(),
            }
            .to_string(),
            skipped_targets: unavailable_source_count(preprocessed, &targets),
            targets,
        });
    }

    Ok(match grouping {
        LibraryGrouping::Automatic => CatalogPlan::AutomaticRoot(targets),
        LibraryGrouping::Configured => CatalogPlan::ConfiguredRoot(targets),
        LibraryGrouping::None => CatalogPlan::Interleaved(targets),
    })
}

fn available_sessions(
    preprocessed: &OperationRequest,
) -> Result<Vec<(AuthorizationSession, Server)>, ApplicationError> {
    let sessions = preprocessed
        .context
        .backends()
        .sessions
        .as_ref()
        .ok_or_else(|| {
            ApplicationError::MissingIdentity("catalog sessions")
                .at(FailureStage::CallerAuthentication)
        })?;
    let mut seen_servers = HashSet::new();
    let sessions = sessions
        .iter()
        .filter(|(_, server)| seen_servers.insert(server.id))
        .cloned()
        .collect::<Vec<_>>();

    if sessions.is_empty() {
        Err(ApplicationError::MissingIdentity("active catalog session")
            .at(FailureStage::CallerAuthentication))
    } else {
        Ok(sessions)
    }
}

fn parent_id(url: &url::Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("ParentId"))
        .map(|(_, value)| value.into_owned())
}

fn series_parent_id(url: &url::Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| {
            key.eq_ignore_ascii_case("SeriesId") || key.eq_ignore_ascii_case("SeasonId")
        })
        .map(|(_, value)| value.into_owned())
}

async fn resolve_aggregate_plan(
    state: &CatalogService,
    preprocessed: &OperationRequest,
    parent_id: &str,
) -> Result<Option<CatalogPlan>, ApplicationError> {
    // A collapsed series/season (aggregate version group) fans out to its
    // member items so seasons/episodes merge across backends the same way
    // movies merge at the library level.
    let Some(group) = state
        .media_storage
        .get_media_version_group(parent_id)
        .await
        .map_err(ApplicationError::Storage)?
    else {
        return Ok(None);
    };
    let members = state
        .media_storage
        .get_media_version_members(group.id)
        .await
        .map_err(ApplicationError::Storage)?;
    if members.is_empty() {
        return Ok(None);
    }
    let sessions = available_sessions(preprocessed)?;
    let mut skipped_targets = 0;
    let mut targets = Vec::new();
    for member in members {
        if preprocessed
            .context
            .access_scope()
            .is_some_and(|scope| !scope.allows(member.server.id))
        {
            continue;
        }
        let Some((session, server)) = sessions
            .iter()
            .find(|(_, server)| server.id == member.server.id)
            .cloned()
        else {
            skipped_targets += 1;
            continue;
        };
        targets.push(CatalogFetchTarget {
            session,
            server,
            parent: Some(ResolvedCatalogParent {
                backend: member.mapping.original_media_id.into(),
                member: member.mapping.virtual_media_id.into(),
                requested: parent_id.into(),
            }),
        });
    }
    if targets.is_empty() {
        return Ok(None);
    }
    let viewer = preprocessed
        .context
        .access_scope()
        .map(|scope| scope.user_id())
        .unwrap_or("anonymous");
    debug!(
        "ParentId {} is version aggregate '{}' - fanning out to {} members",
        parent_id,
        group.virtual_media_id,
        targets.len()
    );
    Ok(Some(CatalogPlan::Virtual {
        catalog_scope_key: MediaCatalogScope {
            kind: MediaScopeKind::Aggregate,
            viewer,
            resource_id: &group.virtual_media_id,
        }
        .to_string(),
        targets,
        skipped_targets,
    }))
}

async fn is_single_server_parent(
    state: &CatalogService,
    parent_id: &str,
) -> Result<bool, ApplicationError> {
    let parent_id = normalize_library_id(parent_id);
    Ok(state
        .media_storage
        .get_media_mapping_by_virtual(&parent_id)
        .await
        .map_err(ApplicationError::Storage)?
        .is_some())
}

pub(super) fn is_library_root_request(url: &url::Url, url_prefix: Option<&str>) -> bool {
    let prefixed_path;
    let path = if let Some(url_prefix) = url_prefix {
        prefixed_path = format!("/{url_prefix}");
        url.path()
            .strip_prefix(&prefixed_path)
            .unwrap_or(url.path())
    } else {
        url.path()
    };
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    match segments.as_slice() {
        [user_views] => user_views.eq_ignore_ascii_case("UserViews"),
        [library, media_folders] => {
            library.eq_ignore_ascii_case("Library")
                && media_folders.eq_ignore_ascii_case("MediaFolders")
        }
        [users, _, views] => {
            users.eq_ignore_ascii_case("Users") && views.eq_ignore_ascii_case("Views")
        }
        _ => false,
    }
}

// Library eligibility and grouping used by planning, roots, and presentation.
pub(super) fn is_live_tv_user_view(item: &MediaItem) -> bool {
    item.collection_type == Some(CollectionType::LiveTv) && item.item_type == BaseItemKind::UserView
}

pub(super) fn automatic_library_key(item: &MediaItem) -> Option<String> {
    let collection_type = presentable_library_collection_type(item)?;
    let name = item.name.as_deref().unwrap_or("").to_lowercase();
    Some(format!("{}:{name}", collection_type.as_str()))
}

pub(super) fn presentable_library_collection_type(item: &MediaItem) -> Option<&CollectionType> {
    let is_library = matches!(
        item.item_type,
        BaseItemKind::UserView | BaseItemKind::CollectionFolder
    );
    item.collection_type
        .as_ref()
        .filter(|collection_type| is_library && **collection_type != CollectionType::LiveTv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unassigned_library_uses_automatic_grouping() {
        let mut item: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "library-id",
            "Type": "CollectionFolder",
            "CollectionType": "movies",
        }))
        .unwrap();
        item.name = Some("Anime".to_string());
        assert_eq!(
            automatic_library_key(&item).as_deref(),
            Some("movies:anime")
        );
    }

    #[test]
    fn parent_id_is_read_case_insensitively() {
        let url = url::Url::parse("http://localhost/Items?parentid=library-id").unwrap();

        assert_eq!(parent_id(&url).as_deref(), Some("library-id"));
    }

    #[test]
    fn parent_id_does_not_match_query_values() {
        let url = url::Url::parse("http://localhost/Items?Filter=ParentId").unwrap();

        assert_eq!(parent_id(&url), None);
    }

    #[test]
    fn library_root_detection_excludes_non_inventory_catalog_requests() {
        let views = url::Url::parse("http://localhost/Users/user/Views").unwrap();
        let user_views = url::Url::parse("http://localhost/UserViews?userId=user").unwrap();
        let resume = url::Url::parse("http://localhost/Users/user/Items/Resume").unwrap();
        let suggestions = url::Url::parse("http://localhost/Items/Suggestions").unwrap();
        let media_folders = url::Url::parse("http://localhost/Library/MediaFolders").unwrap();
        let prefixed_views = url::Url::parse("http://localhost/jellyfin/Users/user/Views").unwrap();

        assert!(is_library_root_request(&views, None));
        assert!(is_library_root_request(&user_views, None));
        assert!(is_library_root_request(&media_folders, None));
        assert!(is_library_root_request(&prefixed_views, Some("jellyfin")));
        assert!(!is_library_root_request(&resume, None));
        assert!(!is_library_root_request(&suggestions, None));
    }
}
