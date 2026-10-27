//! Narrow user-ID resolution shared by request-body and URL translation.
use crate::auth::user_id::UserId;
use crate::{
    auth::{service::AuthorizationSession, UserAuthorizationService},
    servers::id::ServerId,
};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub(crate) enum UserIdError {
    #[error("user mapping storage failed")]
    Storage(#[from] sqlx::Error),
    #[error("Requested user has no mapping on the selected server")]
    MissingMapping,
}

#[derive(Clone)]
pub(super) struct BackendUserIds {
    authorization: Arc<UserAuthorizationService>,
}

impl BackendUserIds {
    pub(super) fn new(authorization: Arc<UserAuthorizationService>) -> Self {
        Self { authorization }
    }

    pub(super) async fn resolve(
        &self,
        user_id: &str,
        session: Option<&AuthorizationSession>,
        server_id: ServerId,
    ) -> Result<String, UserIdError> {
        let user_id = UserId::new(user_id);
        if let Some(session) = session {
            if user_id == session.user_id || user_id == UserId::new(&session.original_user_id) {
                return Ok(session.original_user_id.clone());
            }
        }
        self.authorization
            .get_user_sessions_by_user_id(&user_id)
            .await?
            .and_then(|(_, sessions)| {
                sessions
                    .into_iter()
                    .find(|(_, server)| server.id == server_id)
                    .map(|(session, _)| session.original_user_id)
            })
            .ok_or(UserIdError::MissingMapping)
    }
}
