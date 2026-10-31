//! SyncPlay access-check orchestration. Incoming adapters neither choose whose
//! library access to check nor commit after an unchecked asynchronous gap.
use std::sync::Arc;

use crate::{
    application::sessions::access::MediaAccess,
    sessions::SessionCaller,
    syncplay::{
        models::{GroupInfoDto, PlayRequestDto, QueueRequestDto, SyncPlayGroup},
        service::SyncPlayService,
    },
};

/// Owned handles, built once at startup; clones share the same services.
#[derive(Clone)]
pub(crate) struct SyncPlayOperations {
    pub(crate) groups: Arc<SyncPlayService>,
    access: MediaAccess,
}

pub(crate) enum JoinOutcome {
    Accepted,
    AccessDenied,
    Rejected,
}
impl SyncPlayOperations {
    pub(crate) fn new(groups: Arc<SyncPlayService>, access: MediaAccess) -> Self {
        Self { groups, access }
    }

    async fn group_allows(
        &self,
        group: &SyncPlayGroup,
        items: &[String],
    ) -> Result<bool, sqlx::Error> {
        for user in group.participant_user_ids() {
            if !self.access.allows(&user, items, None).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) async fn set_queue(
        &self,
        session: &SessionCaller,
        payload: PlayRequestDto,
    ) -> Result<(), sqlx::Error> {
        let mut expected = None;
        if let Some(group) = self
            .groups
            .get_group_snapshot_for_session(&session.session_id)
            .await
        {
            if !self.group_allows(&group, &payload.playing_queue).await? {
                self.groups
                    .send_library_access_denied_to_session(&session.session_id)
                    .await;
                return Ok(());
            }
            expected = Some(group.queue_access_snapshot());
        }
        self.groups
            .set_queue(session, payload, expected, chrono::Utc::now())
            .await;
        Ok(())
    }

    pub(crate) async fn queue_items(
        &self,
        session: &SessionCaller,
        payload: QueueRequestDto,
    ) -> Result<(), sqlx::Error> {
        let mut expected = None;
        if let Some(group) = self
            .groups
            .get_group_snapshot_for_session(&session.session_id)
            .await
        {
            if !self.group_allows(&group, &payload.item_ids).await? {
                self.groups
                    .send_library_access_denied_to_session(&session.session_id)
                    .await;
                return Ok(());
            }
            expected = Some(group.queue_access_snapshot());
        }
        self.groups.queue_items(session, payload, expected).await;
        Ok(())
    }

    pub(crate) async fn join(
        &self,
        session: &SessionCaller,
        id: uuid::Uuid,
    ) -> Result<JoinOutcome, sqlx::Error> {
        let mut expected = None;
        if let Some(group) = self.groups.get_group_snapshot_by_id(id).await {
            let items = group.queue_item_ids();
            if !self.access.allows(&session.user.id, &items, None).await? {
                self.groups
                    .send_library_access_denied_to_session(&session.session_id)
                    .await;
                return Ok(JoinOutcome::AccessDenied);
            }
            expected = Some(items);
        }
        Ok(if self.groups.join_group(session, id, expected).await {
            JoinOutcome::Accepted
        } else {
            JoinOutcome::Rejected
        })
    }

    pub(crate) async fn list(
        &self,
        session: &SessionCaller,
    ) -> Result<Vec<GroupInfoDto>, sqlx::Error> {
        let mut groups = Vec::new();
        for group in self.groups.list_group_snapshots().await {
            if self
                .access
                .allows(&session.user.id, &group.queue_item_ids(), None)
                .await?
            {
                groups.push(group.to_group_info());
            }
        }
        Ok(groups)
    }

    pub(crate) async fn get(
        &self,
        session: &SessionCaller,
        id: uuid::Uuid,
    ) -> Result<Option<GroupInfoDto>, sqlx::Error> {
        let Some(group) = self.groups.get_group_snapshot_by_id(id).await else {
            return Ok(None);
        };
        if !self
            .access
            .allows(&session.user.id, &group.queue_item_ids(), None)
            .await?
        {
            return Ok(None);
        }
        Ok(Some(group.to_group_info()))
    }
}
