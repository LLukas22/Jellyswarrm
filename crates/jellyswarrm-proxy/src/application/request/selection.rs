//! Client identity routing and availability policy. Translation receives a
//! selected server; only this application capability chooses among candidates.
use super::routing::RoutingError;
use crate::{
    domain::ids::ServerId,
    libraries::service::{
        VirtualLibraryAccessScope, VirtualLibraryResolution, VirtualLibraryService,
    },
    media::storage::{MediaMapping, MediaStorageService, MediaVersionMember},
    processors::url_processor::selected_media_source,
    protocol::routes::{matches_case_insensitive, MEDIA_ID_PATH_TAGS, MEDIA_ID_QUERY_TAGS},
    protocol::url::contains_id,
    servers::compare_routes,
    servers::{Server, ServerStorageService},
};
type Result<T> = std::result::Result<T, RoutingError>;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct UrlSelection {
    media: Arc<MediaStorageService>,
    servers: Arc<ServerStorageService>,
    libraries: Arc<VirtualLibraryService>,
}
impl UrlSelection {
    pub(crate) fn new(
        media: Arc<MediaStorageService>,
        servers: Arc<ServerStorageService>,
        libraries: Arc<VirtualLibraryService>,
    ) -> Self {
        Self {
            media,
            servers,
            libraries,
        }
    }

    pub(crate) async fn validate_media_access(
        &self,
        id: &str,
        scope: &VirtualLibraryAccessScope,
    ) -> Result<()> {
        if let Some(mapping) = self.media.get_media_mapping_by_virtual(id).await? {
            if !scope.allows(mapping.server_id) {
                return Err(RoutingError::AccessDenied);
            }
        }
        let members = self
            .media
            .get_media_version_members_by_virtual_id(id)
            .await?;
        if !members.is_empty() && members.iter().all(|member| !scope.allows(member.server.id)) {
            return Err(RoutingError::AccessDenied);
        }
        Ok(())
    }

    pub(crate) async fn server_from_client_url(
        &self,
        url: &url::Url,
        scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<Server>> {
        if let Some(scope) = scope {
            for tag in MEDIA_ID_PATH_TAGS {
                if let Some(id) = contains_id(url, tag) {
                    self.validate_media_access(&id, scope).await?;
                }
            }
            for (key, value) in url.query_pairs() {
                if matches_case_insensitive(&key, MEDIA_ID_QUERY_TAGS) {
                    for id in value.split(',') {
                        self.validate_media_access(id, scope).await?;
                    }
                }
            }
        }
        if let Some(source) = selected_media_source(url).map_err(RoutingError::InvalidSelection)? {
            let mut aggregate = None;
            for tag in MEDIA_ID_PATH_TAGS {
                if let Some(id) = contains_id(url, tag) {
                    if let Some(group) = self.media.get_media_version_group(&id).await? {
                        aggregate = Some(group);
                        break;
                    }
                }
            }
            if let Some(group) = aggregate {
                let route = self
                    .media
                    .get_media_version_source_route(group.id, &source)
                    .await?
                    .ok_or_else(|| {
                        RoutingError::InvalidSelection(anyhow::anyhow!(
                            "media source does not belong to the requested media aggregate"
                        ))
                    })?;
                if scope.is_some_and(|scope| !scope.allows(route.member_mapping.server_id)) {
                    return Err(RoutingError::AccessDenied);
                }
                return Ok(self
                    .servers
                    .get_server_by_id(route.member_mapping.server_id)
                    .await?);
            }
            if let Some(server) = self.server_from_media_id(&source, scope).await? {
                return Ok(Some(server));
            }
        }
        for tag in MEDIA_ID_PATH_TAGS {
            if let Some(id) = contains_id(url, tag) {
                if let Some(server) = self.server_from_media_id(&id, scope).await? {
                    return Ok(Some(server));
                }
            }
        }
        for (key, value) in url.query_pairs() {
            if matches_case_insensitive(&key, MEDIA_ID_QUERY_TAGS) {
                for id in value.split(',').map(str::trim).filter(|id| !id.is_empty()) {
                    if let Some(server) = self.server_from_media_id(id, scope).await? {
                        return Ok(Some(server));
                    }
                }
            }
        }
        Ok(None)
    }

    async fn server_from_media_id(
        &self,
        id: &str,
        scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<Server>> {
        if let Some((_, server)) = self.media.get_media_mapping_with_server(id).await? {
            if !allowed(server.id, scope, None) {
                return Err(RoutingError::AccessDenied);
            }
            return Ok(Some(server));
        }
        if let Some(member) = self.media_version_member(id, scope, None).await? {
            return Ok(Some(member.server));
        }
        if self.media.get_media_version_group(id).await?.is_some() {
            let members = self
                .media
                .get_media_version_members_by_virtual_id(id)
                .await?;
            if scope
                .is_some_and(|scope| members.iter().all(|member| !scope.allows(member.server.id)))
            {
                return Err(RoutingError::AccessDenied);
            }
            return Err(RoutingError::UnavailableOwner);
        }
        if let Some(target) = self.libraries.routing_target(id, scope, None).await? {
            return Ok(Some(target.server));
        }
        match self.libraries.resolve(id, scope).await? {
            VirtualLibraryResolution::Unknown | VirtualLibraryResolution::Empty { .. } => Ok(None),
            VirtualLibraryResolution::Resolved(_) => Err(RoutingError::UnavailableOwner),
        }
    }

    pub(crate) async fn client_media_mapping(
        &self,
        id: &str,
        scope: Option<&VirtualLibraryAccessScope>,
        server: Option<ServerId>,
    ) -> std::result::Result<Option<MediaMapping>, sqlx::Error> {
        if let Some(mapping) = self.media.get_media_mapping_by_virtual(id).await? {
            return Ok(allowed(mapping.server_id, scope, server).then_some(mapping));
        }
        if let Some(member) = self.media_version_member(id, scope, server).await? {
            return Ok(Some(member.mapping));
        }
        Ok(self
            .libraries
            .routing_target(id, scope, server)
            .await?
            .map(|target| target.mapping))
    }

    async fn media_version_member(
        &self,
        id: &str,
        scope: Option<&VirtualLibraryAccessScope>,
        server: Option<ServerId>,
    ) -> std::result::Result<Option<MediaVersionMember>, sqlx::Error> {
        let members = self
            .media
            .get_media_version_members_by_virtual_id(id)
            .await?;
        let mut healthy = Vec::new();
        for member in members {
            if allowed(member.server.id, scope, server)
                && self
                    .servers
                    .server_status(member.server.id)
                    .await
                    .is_healthy()
            {
                healthy.push(member);
            }
        }
        Ok(healthy.into_iter().max_by(|left, right| {
            compare_routes(
                &left.server,
                &left.mapping.original_media_id,
                &right.server,
                &right.mapping.original_media_id,
            )
        }))
    }
}

fn allowed(
    server: ServerId,
    scope: Option<&VirtualLibraryAccessScope>,
    required: Option<ServerId>,
) -> bool {
    required.is_none_or(|required| required == server)
        && scope.is_none_or(|scope| scope.allows(server))
}
