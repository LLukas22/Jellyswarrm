//! Store playback sessions and revision-fenced live-stream bindings, not client presence.
use std::{
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use tokio::sync::{RwLock, RwLockWriteGuard};

use crate::auth::user_id::UserId;
use crate::playback::identity::{
    BackendLiveStreamId, PlaybackSessionId, ViewerId, VirtualLiveStreamId,
};
use crate::servers::id::ServerId;

const PLAYBACK_SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRevision(uuid::Uuid);

#[derive(Debug, Clone)]
pub struct PlaybackSession {
    pub session_id: String, // Unique identifier for the session
    pub item_id: String,    // ID of the media item being played
    pub user_id: UserId,
    pub server_id: ServerId,
    /// Exact backend item selected by PlaybackInfo, shared by its item aliases.
    pub original_item_id: Option<String>,
}

/// A snapshot of one binding and its fencing token. Only storage can pair them;
/// consumers cannot replace the session while retaining an unrelated revision.
#[derive(Debug, Clone)]
pub struct RevisionedPlaybackSession {
    session: PlaybackSession,
    revision: SessionRevision,
}

impl RevisionedPlaybackSession {
    pub fn session(&self) -> &PlaybackSession {
        &self.session
    }
    #[cfg(test)]
    pub fn revision(&self) -> SessionRevision {
        self.revision
    }

    #[cfg(test)]
    pub fn into_parts(self) -> (PlaybackSession, SessionRevision) {
        (self.session, self.revision)
    }
}

#[cfg(test)]
impl From<PlaybackSession> for RevisionedPlaybackSession {
    fn from(session: PlaybackSession) -> Self {
        Self {
            session,
            revision: SessionRevision(uuid::Uuid::new_v4()),
        }
    }
}

pub struct SessionStorage {
    sessions: RwLock<Vec<TrackedPlaybackSession>>,
    live_streams: RwLock<LiveStreams>,
    session_ttl: Duration,
}

#[derive(Default)]
struct LiveStreams {
    bindings: Vec<LiveStreamBinding>,
    pending: Vec<Weak<PreparedLiveStream>>,
}

/// An unpublished identity reservation. Dropping it never changes a binding.
pub struct PreparedLiveStream {
    binding: LiveStreamBinding,
}

impl PreparedLiveStream {
    pub fn virtual_id(&self) -> &str {
        self.binding.virtual_id.as_str()
    }
}

#[derive(Clone)]
pub struct LiveStreamBinding {
    pub virtual_id: VirtualLiveStreamId,
    pub original_id: BackendLiveStreamId,
    pub user_id: ViewerId,
    pub server_id: ServerId,
    updated_at: Instant,
}

struct TrackedPlaybackSession {
    session: PlaybackSession,
    revision: SessionRevision,
    updated_at: Instant,
}

impl Default for SessionStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStorage {
    // Historical fixtures keep wire-string convenience methods. Production APIs
    // require distinct ID namespaces, especially for session/viewer arguments.
    #[cfg(test)]
    pub async fn register_live_stream(
        &self,
        original: &str,
        viewer: &str,
        server: ServerId,
    ) -> String {
        self.register_live_binding(&original.into(), &viewer.into(), server)
            .await
            .as_str()
            .to_owned()
    }
    #[cfg(test)]
    pub async fn prepare_live_stream(
        &self,
        original: &str,
        viewer: &str,
        server: ServerId,
    ) -> Arc<PreparedLiveStream> {
        self.prepare_live_binding(&original.into(), &viewer.into(), server)
            .await
    }
    #[cfg(test)]
    pub async fn live_stream(&self, id: &str, viewer: &str) -> Option<LiveStreamBinding> {
        self.live_binding(&id.into(), &viewer.into()).await
    }
    #[cfg(test)]
    pub async fn remove_live_stream(&self, id: &str, viewer: &str) {
        self.close_live_binding(&id.into(), &viewer.into()).await;
    }
    #[cfg(test)]
    pub async fn resolve_report_session(
        &self,
        id: &str,
        viewer: &str,
    ) -> anyhow::Result<Option<RevisionedPlaybackSession>> {
        self.resolve_report_binding(&id.into(), &viewer.into())
            .await
    }
    pub fn new() -> Self {
        Self::with_session_ttl(PLAYBACK_SESSION_TTL)
    }

    pub fn with_session_ttl(session_ttl: Duration) -> Self {
        SessionStorage {
            sessions: RwLock::new(Vec::new()),
            live_streams: RwLock::new(LiveStreams::default()),
            session_ttl,
        }
    }

    pub async fn add_session(&self, mut session: PlaybackSession) {
        session.item_id =
            crate::media::storage::MediaStorageService::normalize_uuid(&session.item_id);
        let mut sessions = self.live_sessions().await;
        if session.original_item_id.is_none() {
            session.original_item_id = sessions
                .iter()
                .find(|tracked| {
                    tracked.session.session_id == session.session_id
                        && tracked.session.user_id == session.user_id
                        && tracked.session.server_id == session.server_id
                })
                .and_then(|tracked| tracked.session.original_item_id.clone());
        }
        let revision = SessionRevision(uuid::Uuid::new_v4());

        sessions.retain(|tracked| {
            let same_authority = tracked.session.session_id == session.session_id
                && tracked.session.user_id == session.user_id;
            !same_authority
                || (tracked.session.server_id == session.server_id
                    && tracked.session.item_id != session.item_id)
        });

        // Retained item aliases share one binding revision, even on identical upserts.
        for tracked in sessions.iter_mut().filter(|tracked| {
            tracked.session.session_id == session.session_id
                && tracked.session.user_id == session.user_id
        }) {
            tracked.revision = revision;
        }
        sessions.push(TrackedPlaybackSession {
            session,
            revision,
            updated_at: Instant::now(),
        });
    }

    pub async fn sessions_for_user(&self, user_id: &str) -> Vec<PlaybackSession> {
        let user_id = UserId::new(user_id);
        self.live_sessions()
            .await
            .iter()
            .filter(|tracked| tracked.session.user_id == user_id)
            .map(|tracked| tracked.session.clone())
            .collect()
    }

    /// Live IDs are backend-local, and may collide across servers. Keep their
    /// caller-scoped bindings independently of stop reports so close can follow stop.
    pub async fn register_live_binding(
        &self,
        original_id: &BackendLiveStreamId,
        user_id: &ViewerId,
        server_id: ServerId,
    ) -> VirtualLiveStreamId {
        loop {
            let plan = self
                .prepare_live_binding(original_id, user_id, server_id)
                .await;
            if self.commit_live_streams(std::slice::from_ref(&plan)).await {
                return plan.binding.virtual_id.clone();
            }
        }
    }

    pub async fn prepare_live_binding(
        &self,
        original_id: &BackendLiveStreamId,
        user_id: &ViewerId,
        server_id: ServerId,
    ) -> Arc<PreparedLiveStream> {
        let mut streams = self.live_streams.write().await;
        streams.pending.retain(|plan| plan.strong_count() > 0);
        let matches = |stream: &LiveStreamBinding| {
            &stream.original_id == original_id
                && &stream.user_id == user_id
                && stream.server_id == server_id
        };
        if let Some(plan) = streams
            .pending
            .iter()
            .filter_map(Weak::upgrade)
            .find(|plan| matches(&plan.binding))
        {
            return plan;
        }
        let virtual_id = streams
            .bindings
            .iter()
            .find(|stream| matches(stream) && stream.updated_at.elapsed() <= self.session_ttl)
            .map(|stream| stream.virtual_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string().into());
        let plan = Arc::new(PreparedLiveStream {
            binding: LiveStreamBinding {
                virtual_id,
                original_id: original_id.clone(),
                user_id: user_id.clone(),
                server_id,
                updated_at: Instant::now(),
            },
        });
        streams.pending.push(Arc::downgrade(&plan));
        plan
    }

    /// Revalidate every reservation before publishing any of them. Close invalidates
    /// outstanding reservations, so a late response cannot resurrect a closed ID.
    pub async fn commit_live_streams(&self, plans: &[Arc<PreparedLiveStream>]) -> bool {
        let mut streams = self.live_streams.write().await;
        if plans.iter().any(|plan| {
            !streams
                .pending
                .iter()
                .any(|pending| pending.ptr_eq(&Arc::downgrade(plan)))
        }) {
            return false;
        }
        // Preparation must not mutate published bindings. Reclaim expired entries
        // only after the complete reservation batch has passed validation; keep
        // reservations intact so another prepared operation can still commit.
        streams
            .bindings
            .retain(|binding| binding.updated_at.elapsed() <= self.session_ttl);
        for plan in plans {
            if let Some(binding) = streams
                .bindings
                .iter_mut()
                .find(|binding| binding.virtual_id.as_str() == plan.virtual_id())
            {
                binding.updated_at = Instant::now();
            } else {
                let mut binding = plan.binding.clone();
                binding.updated_at = Instant::now();
                streams.bindings.push(binding);
            }
        }
        true
    }

    pub async fn live_binding(
        &self,
        id: &VirtualLiveStreamId,
        user_id: &ViewerId,
    ) -> Option<LiveStreamBinding> {
        let mut streams = self.live_streams.write().await;
        streams
            .bindings
            .retain(|stream| stream.updated_at.elapsed() <= self.session_ttl);
        let stream = streams
            .bindings
            .iter_mut()
            .find(|stream| &stream.virtual_id == id && &stream.user_id == user_id)?;
        stream.updated_at = Instant::now();
        Some(stream.clone())
    }

    pub async fn close_live_binding(&self, id: &VirtualLiveStreamId, user_id: &ViewerId) {
        let mut streams = self.live_streams.write().await;
        streams
            .bindings
            .retain(|stream| &stream.virtual_id != id || &stream.user_id != user_id);
        streams.pending.retain(|pending| {
            pending.upgrade().is_some_and(|plan| {
                &plan.binding.virtual_id != id || &plan.binding.user_id != user_id
            })
        });
    }

    pub async fn bind_original_item(
        &self,
        session_id: &str,
        user_id: &str,
        server_id: ServerId,
        original_item_id: &str,
    ) {
        let user_id = UserId::new(user_id);
        let mut sessions = self.live_sessions().await;
        let revision = SessionRevision(uuid::Uuid::new_v4());
        for tracked in sessions.iter_mut().filter(|tracked| {
            tracked.session.session_id == session_id
                && tracked.session.user_id == user_id
                && tracked.session.server_id == server_id
        }) {
            tracked.session.original_item_id = Some(original_item_id.to_string());
            tracked.revision = revision;
        }
    }

    pub async fn get_session(&self, session_id: &str) -> Option<PlaybackSession> {
        let sessions = self.live_sessions().await;

        sessions
            .iter()
            .rev()
            .find(|tracked| tracked.session.session_id == session_id)
            .map(|tracked| tracked.session.clone())
    }

    pub async fn get_session_for_user(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> Option<PlaybackSession> {
        let user_id = UserId::new(user_id);
        let sessions = self.live_sessions().await;

        sessions
            .iter()
            .rev()
            .find(|tracked| {
                tracked.session.session_id == session_id && tracked.session.user_id == user_id
            })
            .map(|tracked| tracked.session.clone())
    }

    pub async fn resolve_report_binding(
        &self,
        session_id: &PlaybackSessionId,
        user_id: &ViewerId,
    ) -> anyhow::Result<Option<RevisionedPlaybackSession>> {
        let sessions = self.live_sessions().await;
        let matching = sessions
            .iter()
            .rev()
            .filter(|tracked| tracked.session.session_id == session_id.as_str());
        if let Some(tracked) = matching
            .clone()
            .find(|tracked| tracked.session.user_id.as_str() == user_id.as_str())
        {
            return Ok(Some(RevisionedPlaybackSession {
                session: tracked.session.clone(),
                revision: tracked.revision,
            }));
        }
        if matching.count() != 0 {
            anyhow::bail!("playback session does not belong to the user");
        }
        Ok(None)
    }

    pub async fn refresh_revisioned_session(&self, binding: &RevisionedPlaybackSession) -> bool {
        self.refresh_session_for_user(
            &binding.session.session_id,
            &binding.session.user_id,
            binding.revision,
        )
        .await
    }

    async fn refresh_session_for_user(
        &self,
        session_id: &str,
        user_id: &str,
        revision: SessionRevision,
    ) -> bool {
        let user_id = UserId::new(user_id);
        let mut sessions = self.live_sessions().await;
        let now = Instant::now();
        let mut refreshed = false;
        for tracked in sessions.iter_mut().filter(|tracked| {
            tracked.session.session_id == session_id
                && tracked.session.user_id == user_id
                && tracked.revision == revision
        }) {
            tracked.updated_at = now;
            refreshed = true;
        }
        refreshed
    }

    pub async fn get_session_by_session_and_item_id(
        &self,
        session_id: &str,
        item_id: &str,
    ) -> Option<PlaybackSession> {
        let item_id = crate::media::storage::MediaStorageService::normalize_uuid(item_id);
        let sessions = self.live_sessions().await;

        sessions
            .iter()
            .rev()
            .find(|tracked| {
                tracked.session.session_id == session_id && tracked.session.item_id == item_id
            })
            .map(|tracked| tracked.session.clone())
    }

    pub async fn get_sessions_by_item_id(&self, item_id: &str) -> Vec<PlaybackSession> {
        let item_id = crate::media::storage::MediaStorageService::normalize_uuid(item_id);
        let sessions = self.live_sessions().await;

        sessions
            .iter()
            .rev()
            .filter(|tracked| tracked.session.item_id == item_id)
            .map(|tracked| tracked.session.clone())
            .collect()
    }

    pub async fn remove_session(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|tracked| tracked.session.session_id != session_id);
    }

    pub async fn remove_revisioned_session(&self, binding: &RevisionedPlaybackSession) {
        self.remove_session_for_user(
            &binding.session.session_id,
            &binding.session.user_id,
            binding.revision,
        )
        .await;
    }

    async fn remove_session_for_user(
        &self,
        session_id: &str,
        user_id: &str,
        revision: SessionRevision,
    ) {
        let user_id = UserId::new(user_id);
        let mut sessions = self.sessions.write().await;
        sessions.retain(|tracked| {
            tracked.session.session_id != session_id
                || tracked.session.user_id != user_id
                || tracked.revision != revision
        });
    }

    pub async fn remove_sessions_for_server(&self, server_id: ServerId) {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|tracked| tracked.session.server_id != server_id);
    }

    async fn live_sessions(&self) -> RwLockWriteGuard<'_, Vec<TrackedPlaybackSession>> {
        let now = Instant::now();
        let mut sessions = self.sessions.write().await;
        Self::prune_stale_sessions(&mut sessions, self.session_ttl, now);
        sessions
    }

    fn prune_stale_sessions(
        sessions: &mut Vec<TrackedPlaybackSession>,
        session_ttl: Duration,
        now: Instant,
    ) {
        sessions.retain(|tracked| now.duration_since(tracked.updated_at) <= session_ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn user_uuid_formats_share_playback_and_live_stream_bindings() {
        let storage = SessionStorage::new();
        let id = uuid::Uuid::new_v4();
        let canonical = id.simple().to_string();
        let alternate = id.hyphenated().to_string().to_uppercase();
        let server = ServerId::new(1);
        storage
            .add_session(PlaybackSession {
                session_id: "play".into(),
                item_id: "item".into(),
                user_id: canonical.as_str().into(),
                server_id: server,
                original_item_id: None,
            })
            .await;
        assert!(storage
            .get_session_for_user("play", &alternate)
            .await
            .is_some());
        assert_eq!(storage.sessions_for_user(&alternate).await.len(), 1);
        storage
            .bind_original_item("play", &alternate, server, "backend-item")
            .await;
        let binding = storage
            .resolve_report_binding(&"play".into(), &alternate.as_str().into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            binding.session().original_item_id.as_deref(),
            Some("backend-item")
        );
        assert!(storage.refresh_revisioned_session(&binding).await);
        storage.remove_revisioned_session(&binding).await;
        assert!(storage.get_session("play").await.is_none());

        let live = storage
            .register_live_stream("backend-live", &canonical, server)
            .await;
        assert_eq!(
            storage
                .register_live_stream("backend-live", &alternate, server)
                .await,
            live
        );
        assert!(storage.live_stream(&live, &alternate).await.is_some());
        assert!(storage.live_stream(&live, "other").await.is_none());
        storage.remove_live_stream(&live, &alternate).await;
        assert!(storage.live_stream(&live, &canonical).await.is_none());
    }

    #[tokio::test]
    async fn live_stream_bindings_are_viewer_scoped_and_expire() {
        let storage = SessionStorage::with_session_ttl(Duration::from_secs(10));
        let server = ServerId::new(1);
        let alice = storage
            .register_live_stream("opaque", "alice", server)
            .await;
        let bob = storage.register_live_stream("opaque", "bob", server).await;
        assert_ne!(alice, bob);
        assert!(storage.live_stream(&alice, "bob").await.is_none());
        storage.remove_live_stream(&alice, "bob").await;
        assert!(storage.live_stream(&alice, "alice").await.is_some());
        for binding in storage.live_streams.write().await.bindings.iter_mut() {
            binding.updated_at = Instant::now() - Duration::from_secs(11);
        }
        assert!(storage.live_stream(&alice, "alice").await.is_none());
        assert!(storage.live_stream(&bob, "bob").await.is_none());
    }

    #[tokio::test]
    async fn live_prepare_is_unpublished_and_does_not_refresh_existing_binding() {
        let storage = SessionStorage::new();
        let server = ServerId::new(1);
        let pending = storage.prepare_live_stream("new", "viewer", server).await;
        assert!(storage
            .live_stream(pending.virtual_id(), "viewer")
            .await
            .is_none());
        drop(pending);
        assert!(storage.live_streams.read().await.bindings.is_empty());
        let id = storage
            .register_live_stream("existing", "viewer", server)
            .await;
        let before = storage.live_streams.read().await.bindings[0].updated_at;
        let pending = storage
            .prepare_live_stream("existing", "viewer", server)
            .await;
        assert_eq!(pending.virtual_id(), id);
        drop(pending);
        assert_eq!(
            storage.live_streams.read().await.bindings[0].updated_at,
            before
        );
    }

    #[tokio::test]
    async fn successful_live_commit_prunes_expired_bindings_without_lookup() {
        let storage = SessionStorage::with_session_ttl(Duration::from_millis(1));
        let server = ServerId::new(1);
        let expired = storage
            .register_live_stream("opaque", "viewer", server)
            .await;
        // Deterministic short-TTL expiry, without sleeping or triggering lookup cleanup.
        storage.live_streams.write().await.bindings[0].updated_at =
            Instant::now() - Duration::from_secs(1);
        let pending = storage
            .prepare_live_stream("opaque", "viewer", server)
            .await;
        assert_ne!(pending.virtual_id(), expired);
        assert_eq!(storage.live_streams.read().await.bindings.len(), 1);
        let replacement = storage
            .register_live_stream("opaque", "viewer", server)
            .await;
        assert_eq!(replacement, pending.virtual_id());
        let streams = storage.live_streams.read().await;
        assert_eq!(streams.bindings.len(), 1);
        assert_eq!(streams.bindings[0].virtual_id.as_str(), replacement);
        drop(streams);
        assert!(storage.commit_live_streams(&[pending]).await);
        assert_eq!(storage.live_streams.read().await.bindings.len(), 1);
    }

    #[tokio::test]
    async fn rejected_live_commit_does_not_prune_expired_bindings_or_valid_reservations() {
        let storage = SessionStorage::with_session_ttl(Duration::from_millis(1));
        let server = ServerId::new(1);
        storage
            .register_live_stream("expired", "viewer", server)
            .await;
        let before = Instant::now() - Duration::from_secs(1);
        storage.live_streams.write().await.bindings[0].updated_at = before;
        let valid = storage.prepare_live_stream("new", "viewer", server).await;
        let obsolete = storage
            .prepare_live_stream("closed", "viewer", server)
            .await;
        storage
            .remove_live_stream(obsolete.virtual_id(), "viewer")
            .await;
        assert!(
            !storage
                .commit_live_streams(&[valid.clone(), obsolete])
                .await
        );
        let streams = storage.live_streams.read().await;
        assert_eq!(streams.bindings.len(), 1);
        assert_eq!(streams.bindings[0].updated_at, before);
        drop(streams);
        assert!(storage.commit_live_streams(&[valid]).await);
        assert_eq!(storage.live_streams.read().await.bindings.len(), 1);
    }

    #[tokio::test]
    async fn concurrent_live_preparations_share_identity_and_commit_atomically() {
        let storage = SessionStorage::new();
        let server = ServerId::new(1);
        let (first, second) = tokio::join!(
            storage.prepare_live_stream("opaque", "viewer", server),
            storage.prepare_live_stream("opaque", "viewer", server),
        );
        assert_eq!(first.virtual_id(), second.virtual_id());
        assert!(storage.commit_live_streams(&[first]).await);
        assert!(storage.commit_live_streams(&[second]).await);
        assert_eq!(storage.live_streams.read().await.bindings.len(), 1);

        let untouched_id = storage
            .register_live_stream("untouched", "viewer", server)
            .await;
        let untouched = storage
            .prepare_live_stream("untouched", "viewer", server)
            .await;
        let untouched_before = storage
            .live_streams
            .read()
            .await
            .bindings
            .iter()
            .find(|binding| binding.virtual_id.as_str() == untouched_id)
            .unwrap()
            .updated_at;
        let valid = storage.prepare_live_stream("new", "viewer", server).await;
        let obsolete = storage
            .prepare_live_stream("opaque", "viewer", server)
            .await;
        storage
            .remove_live_stream(obsolete.virtual_id(), "viewer")
            .await;
        let replacement = storage
            .register_live_stream("opaque", "viewer", server)
            .await;
        assert_ne!(replacement, obsolete.virtual_id());
        let before = storage
            .live_streams
            .read()
            .await
            .bindings
            .iter()
            .find(|binding| binding.virtual_id.as_str() == replacement)
            .unwrap()
            .updated_at;
        assert!(
            !storage
                .commit_live_streams(&[untouched, valid.clone(), obsolete])
                .await
        );
        assert!(storage
            .live_stream(valid.virtual_id(), "viewer")
            .await
            .is_none());
        let streams = storage.live_streams.read().await;
        assert_eq!(streams.bindings.len(), 2);
        assert_eq!(
            streams
                .bindings
                .iter()
                .find(|binding| binding.virtual_id.as_str() == replacement)
                .unwrap()
                .updated_at,
            before
        );
        assert_eq!(
            streams
                .bindings
                .iter()
                .find(|binding| binding.virtual_id.as_str() == untouched_id)
                .unwrap()
                .updated_at,
            untouched_before
        );
    }

    fn playback_session(
        session_id: &str,
        item_id: &str,
        user_id: &str,
        server_id: i64,
    ) -> PlaybackSession {
        PlaybackSession {
            session_id: session_id.into(),
            item_id: item_id.into(),
            user_id: user_id.into(),
            server_id: ServerId::new(server_id),
            original_item_id: None,
        }
    }

    #[tokio::test]
    async fn stale_reports_leave_replacement_bindings_and_aliases_unchanged() {
        for (case, server_id, item_id) in [
            ("identical upsert", ServerId::new(1), "item"),
            (
                "same-server item alias",
                ServerId::new(1),
                "replacement-item",
            ),
            ("server replacement", ServerId::new(2), "item"),
            (
                "server and item replacement",
                ServerId::new(2),
                "replacement-item",
            ),
        ] {
            let storage = SessionStorage::new();
            storage
                .add_session(playback_session("session", "item", "user", 1))
                .await;
            let (_, old_revision) = storage
                .resolve_report_session("session", "user")
                .await
                .unwrap()
                .unwrap()
                .into_parts();
            storage
                .add_session(playback_session(
                    "session",
                    item_id,
                    "user",
                    server_id.as_i64(),
                ))
                .await;
            assert!(
                !storage
                    .refresh_session_for_user("session", "user", old_revision)
                    .await,
                "{case}: stale refresh before alias"
            );
            storage
                .remove_session_for_user("session", "user", old_revision)
                .await;
            let (replacement, replacement_revision) = storage
                .resolve_report_session("session", "user")
                .await
                .unwrap()
                .unwrap()
                .into_parts();
            assert_ne!(old_revision, replacement_revision, "{case}");
            assert_eq!(replacement.server_id, server_id, "{case}");
            assert_eq!(replacement.item_id, item_id, "{case}");
            storage
                .add_session(playback_session(
                    "session",
                    "alias",
                    "user",
                    server_id.as_i64(),
                ))
                .await;
            let (_, revision) = storage
                .resolve_report_session("session", "user")
                .await
                .unwrap()
                .unwrap()
                .into_parts();
            assert_ne!(old_revision, revision, "{case}");
            let before: Vec<_> = storage
                .sessions
                .read()
                .await
                .iter()
                .map(|tracked| tracked.updated_at)
                .collect();

            assert!(
                !storage
                    .refresh_session_for_user("session", "user", old_revision)
                    .await,
                "{case}: stale refresh after alias"
            );
            storage
                .remove_session_for_user("session", "user", old_revision)
                .await;
            let sessions = storage.sessions.read().await;
            assert_eq!(
                sessions
                    .iter()
                    .map(|tracked| tracked.updated_at)
                    .collect::<Vec<_>>(),
                before,
                "{case}: stale reports must preserve timestamps"
            );
            assert!(
                sessions.iter().all(|tracked| {
                    tracked.revision == revision && tracked.session.server_id == server_id
                }),
                "{case}: alias authority"
            );
            assert!(
                sessions
                    .iter()
                    .any(|tracked| tracked.session.item_id == item_id),
                "{case}: replacement retained"
            );
            assert!(
                sessions
                    .iter()
                    .any(|tracked| tracked.session.item_id == "alias"),
                "{case}: alias retained"
            );
            drop(sessions);

            assert!(
                storage
                    .refresh_session_for_user("session", "user", revision)
                    .await,
                "{case}: current refresh"
            );
            let sessions = storage.sessions.read().await;
            assert!(
                sessions.iter().all(|tracked| {
                    tracked.updated_at == sessions[0].updated_at && tracked.revision == revision
                }),
                "{case}: aliases refreshed together"
            );
            drop(sessions);
            storage
                .remove_session_for_user("session", "user", revision)
                .await;
            assert!(
                storage.sessions.read().await.is_empty(),
                "{case}: current removal"
            );
        }
    }

    #[tokio::test]
    async fn report_lookup_distinguishes_unknown_and_other_user_sessions() {
        let storage = SessionStorage::new();
        assert!(storage
            .resolve_report_session("session", "caller")
            .await
            .unwrap()
            .is_none());
        storage
            .add_session(playback_session("session", "item", "owner", 1))
            .await;
        assert!(storage
            .resolve_report_session("session", "caller")
            .await
            .is_err());
        assert!(storage
            .resolve_report_session("session", "owner")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn test_add_session_upserts_by_session_and_item_id() {
        let storage = SessionStorage::new();

        storage
            .add_session(playback_session("session-1", "item-1", "user-1", 1))
            .await;
        storage
            .add_session(playback_session("session-1", "item-1", "user-1", 2))
            .await;

        let session = storage.get_session("session-1").await.unwrap();
        assert_eq!(session.server_id, ServerId::new(2));
        assert_eq!(
            storage
                .get_session_by_session_and_item_id("session-1", "item-1")
                .await
                .unwrap()
                .server_id,
            ServerId::new(2)
        );
    }

    #[tokio::test]
    async fn reused_user_session_id_has_one_server_authority() {
        let storage = SessionStorage::new();
        storage
            .add_session(playback_session("session-1", "item-1", "user-1", 1))
            .await;
        storage
            .add_session(playback_session("session-1", "item-2", "user-1", 2))
            .await;

        assert!(storage
            .get_session_by_session_and_item_id("session-1", "item-1")
            .await
            .is_none());
        assert_eq!(
            storage
                .get_session_for_user("session-1", "user-1")
                .await
                .unwrap()
                .server_id,
            ServerId::new(2)
        );
    }

    #[tokio::test]
    async fn test_same_item_id_requires_matching_session_id() {
        let storage = SessionStorage::new();

        storage
            .add_session(playback_session("session-1", "shared-item", "user-1", 1))
            .await;
        storage
            .add_session(playback_session("session-2", "shared-item", "user-1", 2))
            .await;

        let session = storage
            .get_session_by_session_and_item_id("session-1", "shared-item")
            .await
            .unwrap();
        assert_eq!(session.server_id, ServerId::new(1));

        let session = storage
            .get_session_by_session_and_item_id("session-2", "shared-item")
            .await
            .unwrap();
        assert_eq!(session.server_id, ServerId::new(2));

        assert!(storage
            .get_session_by_session_and_item_id("missing-session", "shared-item")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_stale_sessions_expire() {
        let storage = SessionStorage::with_session_ttl(Duration::from_millis(1));

        storage
            .add_session(playback_session("session-1", "item-1", "user-1", 1))
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(storage.get_session("session-1").await.is_none());
        assert!(storage
            .get_session_by_session_and_item_id("session-1", "item-1")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_add_session_prunes_stale_sessions() {
        let storage = SessionStorage::with_session_ttl(Duration::from_millis(1));

        storage
            .add_session(playback_session("stale-session", "stale-item", "user-1", 1))
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;

        storage
            .add_session(playback_session("fresh-session", "fresh-item", "user-1", 2))
            .await;

        assert!(storage.get_session("stale-session").await.is_none());
        assert!(storage.get_session("fresh-session").await.is_some());
    }

    #[tokio::test]
    async fn test_remove_sessions_for_server() {
        let storage = SessionStorage::new();

        storage
            .add_session(playback_session("session-1", "item-1", "user-1", 1))
            .await;
        storage
            .add_session(playback_session("session-2", "item-2", "user-1", 2))
            .await;

        storage.remove_sessions_for_server(ServerId::new(1)).await;

        assert!(storage.get_session("session-1").await.is_none());
        assert!(storage.get_session("session-2").await.is_some());
    }

    #[tokio::test]
    async fn test_get_sessions_by_item_id_returns_active_matches_newest_first() {
        let storage = SessionStorage::new();

        storage
            .add_session(playback_session("session-1", "shared-item", "user-1", 1))
            .await;
        storage
            .add_session(playback_session("session-2", "shared-item", "user-2", 2))
            .await;

        let sessions = storage.get_sessions_by_item_id("shared-item").await;
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "session-2");
        assert_eq!(sessions[1].session_id, "session-1");
    }
}
