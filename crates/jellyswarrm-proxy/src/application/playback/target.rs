//! Shared playback authority and identity resolution. Endpoint adapters choose
//! availability/status policy; this module never falls back from an explicit owner.
use crate::{
    application::context::{AuthorizedTarget, RequestContext, TargetError},
    application::playback::sessions::{
        LiveStreamBinding, PlaybackSession, RevisionedPlaybackSession, SessionStorage,
    },
    media::storage::{MediaMapping, MediaStorageService},
    servers::id::ServerId,
    servers::storage::ServerStorageService,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ResolutionError {
    #[error("unknown playback item")]
    UnknownItem,
    #[error("media source does not belong to playback item")]
    InvalidSource,
    #[error("unknown playback source")]
    UnknownSource,
    #[error("conflicting playback item and source servers")]
    SourceConflict,
    #[error("conflicting playback session and item/source authority")]
    BindingConflict,
    #[error("playback item member is unavailable or ambiguous")]
    AmbiguousMember,
    #[error("playback session belongs to another viewer")]
    ViewerMismatch(#[source] anyhow::Error),
    #[error("selected playback owner is offline")]
    OfflineOwner,
    #[error("selected playback owner no longer exists")]
    MissingOwner,
    #[error("unknown live stream binding")]
    UnknownLiveBinding,
    #[error(transparent)]
    Target(#[from] TargetError),
    #[error("playback identity storage failed: {0}")]
    Storage(#[from] sqlx::Error),
}

/// A selected backend and exact item identity, before any URL/credential rewriting.
pub(crate) struct ResolvedMediaTarget {
    pub backend: AuthorizedTarget,
    pub member: Option<MediaMapping>,
}

#[derive(Clone, Copy)]
pub(crate) enum SelectionPolicy {
    Negotiation,
    Direct,
    Resource,
}

pub(crate) struct PlaybackSelectionPlan {
    pub target: ResolvedMediaTarget,
    pub reason: SelectionReason,
}

/// Normalized identity intent; endpoint query/body precedence is resolved by
/// the caller before selection, while a known binding retains its authority.
pub(crate) struct PlaybackIntent<'a> {
    pub item: &'a str,
    pub source: Option<&'a str>,
    pub binding: Option<&'a PlaybackSession>,
}

#[derive(Debug)]
pub(crate) enum SelectionReason {
    KnownBinding,
    ExplicitSource,
    DefaultMapping,
}

pub(crate) struct MediaTargetResolver<'a> {
    pub media: &'a MediaStorageService,
}

impl MediaTargetResolver<'_> {
    /// Shared explicit/session/default precedence. Availability is deliberately
    /// endpoint-specific: direct initial targets retain routing policy, resources
    /// and known bindings require a healthy owner, negotiation checks selections.
    pub async fn plan(
        &self,
        context: &RequestContext,
        initial: AuthorizedTarget,
        intent: PlaybackIntent<'_>,
        servers: &ServerStorageService,
        policy: SelectionPolicy,
    ) -> Result<PlaybackSelectionPlan, ResolutionError> {
        let PlaybackIntent {
            item,
            source,
            binding,
        } = intent;
        let group = self.media.get_media_version_group(item).await?;
        let explicit = source.filter(|source| !source.trim().is_empty());
        let backend = if let Some(binding) = binding {
            if servers.get_server_by_id(binding.server_id).await?.is_none() {
                return Err(ResolutionError::MissingOwner);
            }
            if !servers.server_status(binding.server_id).await.is_healthy() {
                return Err(ResolutionError::OfflineOwner);
            }
            if initial.server().id == binding.server_id {
                initial
            } else {
                context.target(binding.server_id)?
            }
        } else if let (Some(_), Some(source)) = (&group, explicit) {
            let owner = self.owners(item, Some(source), None).await?[0];
            let backend = if initial.server().id == owner {
                initial
            } else {
                context.target(owner)?
            };
            let member = self.member(item, Some(source), None, owner).await?;
            if !servers.server_status(owner).await.is_healthy() {
                return Err(ResolutionError::OfflineOwner);
            }
            return Ok(PlaybackSelectionPlan {
                target: ResolvedMediaTarget {
                    backend,
                    member: Some(member),
                },
                reason: SelectionReason::ExplicitSource,
            });
        } else {
            initial
        };
        if matches!(policy, SelectionPolicy::Resource)
            && !servers
                .server_status(backend.server().id)
                .await
                .is_healthy()
        {
            return Err(ResolutionError::OfflineOwner);
        }
        let member = if let Some(group) = group {
            if binding.is_some_and(|binding| binding.original_item_id.is_some()) {
                Some(
                    self.member(item, explicit, binding, backend.server().id)
                        .await?,
                )
            } else if binding.is_none()
                && servers
                    .server_status(backend.server().id)
                    .await
                    .is_healthy()
            {
                // The same ordering as existing aggregate URL translation, but
                // now fixed before request construction rather than inside it.
                self.media
                    .get_media_version_members(group.id)
                    .await?
                    .into_iter()
                    .filter(|member| member.server.id == backend.server().id)
                    .max_by(|left, right| {
                        crate::libraries::service::compare_virtual_library_routes(
                            &left.server,
                            &left.mapping.original_media_id,
                            &right.server,
                            &right.mapping.original_media_id,
                        )
                    })
                    .map(|member| member.mapping)
            } else {
                None
            }
        } else {
            None
        };
        Ok(PlaybackSelectionPlan {
            target: ResolvedMediaTarget { backend, member },
            reason: if binding.is_some() {
                SelectionReason::KnownBinding
            } else {
                SelectionReason::DefaultMapping
            },
        })
    }

    /// All identity-compatible owners, before availability filtering. Reports
    /// require one available owner; default negotiation uses its ordered policy.
    pub async fn owners(
        &self,
        item: &str,
        source: Option<&str>,
        binding: Option<&PlaybackSession>,
    ) -> Result<Vec<ServerId>, ResolutionError> {
        let mut owners = if let Some(group) = self.media.get_media_version_group(item).await? {
            if let Some(source) = source {
                vec![
                    self.media
                        .get_media_version_source_route(group.id, source)
                        .await?
                        .ok_or(ResolutionError::InvalidSource)?
                        .member_mapping
                        .server_id,
                ]
            } else {
                self.media
                    .get_media_version_members(group.id)
                    .await?
                    .into_iter()
                    .map(|member| member.mapping.server_id)
                    .collect()
            }
        } else {
            let member = self
                .media
                .get_media_mapping_by_virtual(item)
                .await?
                .ok_or(ResolutionError::UnknownItem)?;
            if let Some(source) = source {
                let source = self
                    .media
                    .get_media_mapping_by_virtual(source)
                    .await?
                    .ok_or(ResolutionError::UnknownSource)?;
                if source.server_id != member.server_id {
                    return Err(ResolutionError::SourceConflict);
                }
            }
            vec![member.server_id]
        };
        if let Some(binding) = binding {
            if !owners.contains(&binding.server_id) {
                return Err(ResolutionError::BindingConflict);
            }
            owners.retain(|id| *id == binding.server_id);
        }
        Ok(owners)
    }

    /// Resolve an exact member without conflating upstream source and item IDs.
    pub async fn member(
        &self,
        item: &str,
        source: Option<&str>,
        binding: Option<&PlaybackSession>,
        server: ServerId,
    ) -> Result<MediaMapping, ResolutionError> {
        let member = if let Some(group) = self.media.get_media_version_group(item).await? {
            if let Some(source) = source {
                self.media
                    .get_media_version_source_route(group.id, source)
                    .await?
                    .ok_or(ResolutionError::InvalidSource)?
                    .member_mapping
            } else {
                let bound_item = binding.and_then(|b| b.original_item_id.as_deref());
                let mut members = self
                    .media
                    .get_media_version_members(group.id)
                    .await?
                    .into_iter()
                    .filter(|member| {
                        member.mapping.server_id == server
                            && bound_item.is_none_or(|id| member.mapping.original_media_id == id)
                    });
                let member = members.next().ok_or(ResolutionError::AmbiguousMember)?;
                if members.next().is_some() {
                    return Err(ResolutionError::AmbiguousMember);
                }
                member.mapping
            }
        } else {
            // Also validate source ownership on non-aggregate items.
            self.owners(item, source, binding).await?;
            self.media
                .get_media_mapping_by_virtual(item)
                .await?
                .ok_or(ResolutionError::UnknownItem)?
        };
        if member.server_id != server
            || binding.is_some_and(|binding| {
                binding.server_id != member.server_id
                    || binding
                        .original_item_id
                        .as_ref()
                        .is_some_and(|id| id != &member.original_media_id)
            })
        {
            return Err(ResolutionError::BindingConflict);
        }
        Ok(member)
    }

    #[cfg(test)]
    pub async fn selected(
        &self,
        context: &RequestContext,
        item: &str,
        source: &str,
    ) -> Result<ResolvedMediaTarget, ResolutionError> {
        let owners = self.owners(item, Some(source), None).await?;
        let owner = *owners.first().ok_or(ResolutionError::InvalidSource)?;
        let backend = context.target(owner)?;
        let member = self.member(item, Some(source), None, owner).await?;
        Ok(ResolvedMediaTarget {
            backend,
            member: Some(member),
        })
    }

    /// Explicit known sessions must match a registered item alias and viewer.
    /// Unknown sessions remain unbound; delivery may bind only after success.
    /// Ambiguous implicit matches deliberately return None (mapping fallback).
    pub async fn session(
        &self,
        sessions: &SessionStorage,
        item: &str,
        session: Option<&str>,
        viewer: &str,
        explicit_source: bool,
    ) -> Result<Option<PlaybackSession>, ResolutionError> {
        let candidates = sessions.get_sessions_by_item_id(item).await;
        if let Some(session) = session {
            let known = owned_session(sessions, &session.into(), &viewer.into()).await?;
            let matching = matching_session(candidates, Some(session), Some(viewer)).ok();
            if known.is_some() && matching.is_none() {
                return Err(ResolutionError::BindingConflict);
            }
            return Ok(matching);
        }
        if explicit_source {
            return Ok(None);
        }
        Ok(matching_session(candidates, None, Some(viewer)).ok())
    }
}

/// Shared viewer authority for delivery, live-open and transcoder cancellation.
pub(crate) async fn owned_session(
    sessions: &SessionStorage,
    id: &crate::application::identity::PlaybackSessionId,
    viewer: &crate::application::identity::ViewerId,
) -> Result<Option<RevisionedPlaybackSession>, ResolutionError> {
    sessions
        .resolve_report_binding(id, viewer)
        .await
        .map_err(ResolutionError::ViewerMismatch)
}

/// Live IDs are opaque caller-scoped aliases, not media IDs. Their registered
/// owner, rather than a default server or item lookup, is authoritative.
pub(crate) async fn live_target(
    context: &RequestContext,
    sessions: &SessionStorage,
    id: &crate::application::identity::VirtualLiveStreamId,
    viewer: &crate::application::identity::ViewerId,
) -> Result<(AuthorizedTarget, LiveStreamBinding), ResolutionError> {
    let binding = sessions
        .live_binding(id, viewer)
        .await
        .ok_or(ResolutionError::UnknownLiveBinding)?;
    let backend = context.target(binding.server_id)?;
    Ok((backend, binding))
}

pub(crate) fn matching_session(
    mut candidates: Vec<PlaybackSession>,
    session: Option<&str>,
    viewer: Option<&str>,
) -> Result<PlaybackSession, usize> {
    candidates.retain(|candidate| {
        session.is_none_or(|id| candidate.session_id == id)
            && viewer.is_none_or(|id| candidate.user_id == id)
    });
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        count => Err(count),
    }
}
