//! Resolve authorized virtual libraries and select deterministic backend routes.
use super::*;
use crate::servers::compare_routes;

impl VirtualLibraryService {
    pub(crate) async fn library_grouping(
        &self,
        automatic_merging_enabled: bool,
    ) -> Result<LibraryGrouping, sqlx::Error> {
        if automatic_merging_enabled {
            return Ok(LibraryGrouping::Automatic);
        }

        if self.has_groups().await? {
            Ok(LibraryGrouping::Configured)
        } else {
            Ok(LibraryGrouping::None)
        }
    }

    pub(crate) async fn resolve(
        &self,
        virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<VirtualLibraryResolution, sqlx::Error> {
        if let Some((group, members)) = self.resolve_group(virtual_id, access_scope).await? {
            let viewer = access_scope
                .map(VirtualLibraryAccessScope::user_id)
                .unwrap_or("anonymous");
            return Ok(resolution(
                group.name,
                members,
                crate::media::scope::MediaCatalogScope {
                    kind: crate::media::scope::MediaScopeKind::Configured,
                    viewer,
                    resource_id: &normalize_library_id(&group.virtual_id),
                }
                .to_string(),
            ));
        }

        if let Some((library, members)) = self
            .resolve_automatic_library(virtual_id, access_scope)
            .await?
        {
            let Some(access_scope) = access_scope else {
                return Ok(VirtualLibraryResolution::Empty { name: library.name });
            };
            return Ok(resolution(
                library.name,
                members,
                crate::media::scope::MediaCatalogScope {
                    kind: crate::media::scope::MediaScopeKind::Automatic,
                    viewer: access_scope.user_id(),
                    resource_id: &normalize_library_id(&library.virtual_id),
                }
                .to_string(),
            ));
        }

        Ok(VirtualLibraryResolution::Unknown)
    }

    pub(crate) async fn routing_target(
        &self,
        virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> Result<Option<VirtualLibraryMember>, sqlx::Error> {
        let VirtualLibraryResolution::Resolved(resolved) =
            self.resolve(virtual_id, access_scope).await?
        else {
            return Ok(None);
        };
        Ok(resolved
            .members
            .into_iter()
            .filter(|member| server_is_allowed(member.server.id, access_scope, required_server_id))
            .max_by(|left, right| {
                compare_routes(
                    &left.server,
                    &left.mapping.original_media_id,
                    &right.server,
                    &right.mapping.original_media_id,
                )
            }))
    }

    pub(super) async fn resolve_automatic_library(
        &self,
        virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<(AutomaticVirtualLibrary, Vec<VirtualLibraryMember>)>, sqlx::Error> {
        let Some(library) = self.get_automatic_library(virtual_id).await? else {
            return Ok(None);
        };
        let members = self
            .get_automatic_library_members(&library.virtual_id, access_scope)
            .await?;
        Ok(Some((library, members)))
    }

    pub(super) async fn has_groups(&self) -> Result<bool, sqlx::Error> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM library_groups")
            .fetch_one(&self.pool)
            .await?;
        Ok(count > 0)
    }

    pub(super) async fn resolve_group(
        &self,
        virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<(LibraryGroup, Vec<VirtualLibraryMember>)>, sqlx::Error> {
        let Some(group) = self.get_group(virtual_id).await? else {
            return Ok(None);
        };

        let records = self.list_members(&group.virtual_id).await?;
        if records.is_empty() {
            return Ok(Some((group, Vec::new())));
        }

        let mut members = Vec::new();
        for record in records {
            if !server_is_allowed(record.server_id, access_scope, None) {
                continue;
            }
            // A server mapping alone does not establish access to every library
            // on it. Once a viewer inventory exists, require a compatible member.
            // Before the first inventory, retain routing with upstream enforcement.
            let viewer_kind: Option<(Option<String>,)> = if let Some(scope) = access_scope {
                sqlx::query_as(
                    "SELECT member.collection_type FROM library_viewer_inventories inventory \
                     LEFT JOIN library_viewer_members member ON member.user_id = inventory.user_id \
                       AND member.server_id = inventory.server_id AND member.original_library_id = ? \
                     WHERE inventory.user_id = ? AND inventory.server_id = ?",
                ).bind(&record.original_library_id).bind(scope.user_id()).bind(record.server_id.as_i64())
                    .fetch_optional(&self.pool).await?
            } else {
                None
            };
            if let Some((kind,)) = viewer_kind {
                if !kind.is_some_and(|kind| {
                    group
                        .collection_type
                        .as_deref()
                        .is_some_and(|expected| expected.eq_ignore_ascii_case(&kind))
                }) {
                    continue;
                }
            } else if let Some(discovered) = self
                .get_discovered_library(record.server_id, &record.original_library_id)
                .await?
            {
                if !group.collection_type.as_deref().is_some_and(|kind| {
                    kind.eq_ignore_ascii_case(discovered.collection_type.trim())
                }) {
                    continue;
                }
            }
            let Some(server) = self
                .server_storage
                .get_server_by_id(record.server_id)
                .await?
            else {
                continue;
            };

            let mapping = self
                .media_storage
                .get_or_create_media_mapping(&record.original_library_id, &server)
                .await?;

            members.push(VirtualLibraryMember { mapping, server });
        }

        Ok(Some((group, members)))
    }
}
