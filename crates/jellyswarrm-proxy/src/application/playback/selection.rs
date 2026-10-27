//! Record negotiated playback sources and retain revision-fenced routability.
use crate::application::error::ApplicationError;
use crate::{
    application::details::sources::version_name,
    media::storage::{MediaStorageService, MediaVersionSourceObservation},
    models::MediaSource,
    servers::storage::Server,
};

pub(crate) async fn record_playback_sources(
    media: &MediaStorageService,
    aggregate_id: &str,
    server: &Server,
    original_item_id: &str,
    source_generation: crate::media::storage::ReconciliationGeneration,
    sources: &mut Vec<MediaSource>,
) -> Result<(), ApplicationError> {
    let Some(group) = media
        .get_media_version_group(aggregate_id)
        .await
        .map_err(ApplicationError::Storage)?
    else {
        return Ok(());
    };
    let Some(member) = media
        .get_media_version_members(group.id)
        .await
        .map_err(ApplicationError::Storage)?
        .into_iter()
        .find(|member| {
            member.mapping.server_id == server.id
                && member.mapping.original_media_id == original_item_id
        })
    else {
        return Ok(());
    };
    let observations = sources
        .iter()
        .map(|source| MediaVersionSourceObservation {
            member_mapping_id: member.mapping.id,
            source_virtual_id: source.id.clone(),
        })
        .collect::<Vec<_>>();
    // Keep negotiated names identical to the versions advertised by details.
    for source in sources.iter_mut() {
        source.name = Some(version_name(source, server));
    }
    let sources_recorded = media
        .observe_media_version_sources(
            group.id,
            source_generation,
            &[member.mapping.id],
            &observations,
        )
        .await
        .map_err(ApplicationError::Storage)?;
    if !sources_recorded {
        let mut routable_sources = Vec::new();
        for source in sources.drain(..) {
            if media
                .get_media_version_source_route(group.id, &source.id)
                .await
                .map_err(ApplicationError::Storage)?
                .is_some()
            {
                routable_sources.push(source);
            }
        }
        *sources = routable_sources;
    }
    Ok(())
}
