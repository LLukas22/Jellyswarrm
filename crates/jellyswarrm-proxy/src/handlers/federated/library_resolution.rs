use std::collections::HashSet;

use hyper::StatusCode;
use tracing::{debug, error};

use crate::{
    request_preprocessing::PreprocessedRequest,
    server_storage::Server,
    user_authorization_service::AuthorizationSession,
    virtual_library_service::{normalize_library_id, LibraryGrouping, VirtualLibraryResolution},
    AppState,
};

pub(super) enum CatalogPlan {
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

pub(super) struct CatalogFetchTarget {
    pub(super) session: AuthorizationSession,
    pub(super) server: Server,
    pub(super) parent_id: Option<String>,
}

pub(super) async fn resolve_catalog_plan(
    state: &AppState,
    preprocessed: &PreprocessedRequest,
) -> Result<CatalogPlan, StatusCode> {
    if let Some(parent_id) = parent_id(preprocessed.original_request.url()) {
        let resolution = state
            .virtual_library_service
            .resolve(&parent_id, preprocessed.access_scope.as_ref())
            .await
            .map_err(|error| {
                error!("Failed to resolve virtual library for {parent_id}: {error}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

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
                            parent_id: Some(member.mapping.original_media_id),
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
                if is_single_server_parent(state, &parent_id).await {
                    return Ok(CatalogPlan::SingleServer);
                }
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
            .map_err(|error| {
                error!("Failed to determine library grouping: {error}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
    } else {
        LibraryGrouping::None
    };

    let targets = available_sessions(preprocessed)?
        .into_iter()
        .map(|(session, server)| CatalogFetchTarget {
            session,
            server,
            parent_id: None,
        })
        .collect();

    Ok(match grouping {
        LibraryGrouping::Automatic => CatalogPlan::AutomaticRoot(targets),
        LibraryGrouping::Configured => CatalogPlan::ConfiguredRoot(targets),
        LibraryGrouping::None => CatalogPlan::Interleaved(targets),
    })
}

fn available_sessions(
    preprocessed: &PreprocessedRequest,
) -> Result<Vec<(AuthorizationSession, Server)>, StatusCode> {
    let sessions = preprocessed
        .sessions
        .as_ref()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let mut seen_servers = HashSet::new();
    let sessions = sessions
        .iter()
        .filter(|(_, server)| seen_servers.insert(server.id))
        .cloned()
        .collect::<Vec<_>>();

    if sessions.is_empty() {
        Err(StatusCode::UNAUTHORIZED)
    } else {
        Ok(sessions)
    }
}

fn parent_id(url: &url::Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("ParentId"))
        .map(|(_, value)| value.into_owned())
}

async fn is_single_server_parent(state: &AppState, parent_id: &str) -> bool {
    let parent_id = normalize_library_id(parent_id);
    state
        .media_storage
        .get_media_mapping_by_virtual(&parent_id)
        .await
        .ok()
        .flatten()
        .is_some()
}

fn is_library_root_request(url: &url::Url, url_prefix: Option<&str>) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

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
