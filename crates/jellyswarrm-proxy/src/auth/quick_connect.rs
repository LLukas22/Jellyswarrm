//! Temporary Quick Connect state. Codes approve; secrets poll/redeem. Wire
//! snapshots never expose mutable state, and only one redemption can run at once.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

use chrono::{DateTime, Duration, Utc};
use rand::Rng;
use serde::Serialize;

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ConnectCode(String);
impl From<&str> for ConnectCode {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ConnectSecret(String);
impl From<&str> for ConnectSecret {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Response data only. Changing this snapshot cannot change an approval.
#[derive(Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct QuickConnectSession {
    pub authenticated: bool,
    pub secret: String,
    pub code: String,
    pub device_id: String,
    pub device_name: String,
    pub app_name: String,
    pub app_version: String,
    pub date_added: DateTime<Utc>,
}

enum Stage {
    Pending,
    Approved(String),
    Redeeming(String),
}

struct Session {
    view: QuickConnectSession,
    stage: Stage,
    expires_at: DateTime<Utc>,
}

#[derive(Default)]
struct Sessions {
    by_secret: HashMap<ConnectSecret, Session>,
    by_code: HashMap<ConnectCode, ConnectSecret>,
}

impl Sessions {
    fn remove(&mut self, secret: &ConnectSecret) {
        if let Some(session) = self.by_secret.remove(secret) {
            self.by_code.remove(&ConnectCode(session.view.code));
        }
    }

    fn cleanup(&mut self, now: DateTime<Utc>) -> usize {
        let expired: Vec<_> = self
            .by_secret
            .iter()
            .filter(|(_, session)| now > session.expires_at)
            .map(|(secret, _)| secret.clone())
            .collect();
        for secret in &expired {
            self.remove(secret);
        }
        expired.len()
    }
}

#[derive(Clone, Default)]
pub struct QuickConnectStorage {
    sessions: Arc<Mutex<Sessions>>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RedemptionError {
    #[error("Quick Connect session not found")]
    NotFound,
    #[error("Quick Connect session is not approved or already being redeemed")]
    NotApproved,
}

impl QuickConnectStorage {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn initiate(
        &self,
        device_id: String,
        device_name: String,
        app_name: String,
        app_version: String,
    ) -> QuickConnectSession {
        self.initiate_at(device_id, device_name, app_name, app_version, Utc::now())
    }

    fn initiate_at(
        &self,
        device_id: String,
        device_name: String,
        app_name: String,
        app_version: String,
        now: DateTime<Utc>,
    ) -> QuickConnectSession {
        let mut sessions = self.lock();
        sessions.cleanup(now);
        let code = loop {
            let candidate: String = (0..6)
                .map(|_| char::from(b'0' + rand::rng().random_range(0..10)))
                .collect();
            if !sessions
                .by_code
                .contains_key(&ConnectCode(candidate.clone()))
            {
                break candidate;
            }
        };
        let secret = uuid::Uuid::new_v4().to_string();
        let view = QuickConnectSession {
            authenticated: false,
            secret: secret.clone(),
            code: code.clone(),
            device_id,
            device_name,
            app_name,
            app_version,
            date_added: now,
        };
        let secret = ConnectSecret(secret);
        sessions.by_code.insert(ConnectCode(code), secret.clone());
        sessions.by_secret.insert(
            secret,
            Session {
                view: view.clone(),
                stage: Stage::Pending,
                expires_at: now + Duration::minutes(10),
            },
        );
        view
    }

    pub(crate) fn poll(&self, secret: &ConnectSecret) -> Option<QuickConnectSession> {
        let mut sessions = self.lock();
        sessions.cleanup(Utc::now());
        sessions
            .by_secret
            .get(secret)
            .map(|session| session.view.clone())
    }

    pub(crate) fn approve(&self, code: &ConnectCode, user_id: String) -> bool {
        let mut sessions = self.lock();
        sessions.cleanup(Utc::now());
        let Some(secret) = sessions.by_code.get(code).cloned() else {
            return false;
        };
        let session = sessions
            .by_secret
            .get_mut(&secret)
            .expect("code index refers to a session");
        match &session.stage {
            Stage::Pending => {
                session.stage = Stage::Approved(user_id);
                session.view.authenticated = true;
                true
            }
            Stage::Approved(approved) => approved == &user_id,
            Stage::Redeeming(_) => false,
        }
    }

    pub(crate) fn begin_redemption(
        &self,
        secret: &ConnectSecret,
    ) -> Result<Redemption, RedemptionError> {
        let mut sessions = self.lock();
        sessions.cleanup(Utc::now());
        let session = sessions
            .by_secret
            .get_mut(secret)
            .ok_or(RedemptionError::NotFound)?;
        let Stage::Approved(user_id) = &session.stage else {
            return Err(RedemptionError::NotApproved);
        };
        let user_id = user_id.clone();
        session.stage = Stage::Redeeming(user_id.clone());
        Ok(Redemption {
            storage: self.clone(),
            secret: secret.clone(),
            user_id,
            view: session.view.clone(),
        })
    }

    pub(crate) fn cleanup_expired(&self) -> usize {
        self.lock().cleanup(Utc::now())
    }

    /// The application runtime owns the task running this future.
    pub(crate) async fn cleanup_loop(&self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            self.cleanup_expired();
        }
    }

    fn lock(&self) -> MutexGuard<'_, Sessions> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Reservation across asynchronous backend authentication/registration. The
/// application coordinator must retain this until all workers and submitted
/// writes finish. Only then may Drop restore approval or complete consume it.
pub(crate) struct Redemption {
    storage: QuickConnectStorage,
    secret: ConnectSecret,
    user_id: String,
    view: QuickConnectSession,
}
impl Redemption {
    pub(crate) fn user_id(&self) -> &str {
        &self.user_id
    }
    pub(crate) fn session(&self) -> &QuickConnectSession {
        &self.view
    }
    pub(crate) fn complete(self) {
        self.storage.lock().remove(&self.secret);
    }
}
impl Drop for Redemption {
    fn drop(&mut self) {
        let mut sessions = self.storage.lock();
        if let Some(session) = sessions.by_secret.get_mut(&self.secret) {
            if let Stage::Redeeming(user_id) = &session.stage {
                session.stage = Stage::Approved(user_id.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn initiate(storage: &QuickConnectStorage) -> QuickConnectSession {
        storage.initiate("device".into(), "TV".into(), "App".into(), "1".into())
    }

    #[test]
    fn codes_and_secrets_have_separate_authority_and_snapshots_are_not_state() {
        let storage = QuickConnectStorage::new();
        let mut view = initiate(&storage);
        assert!(storage.poll(&view.code.as_str().into()).is_none());
        assert!(!storage.approve(&view.secret.as_str().into(), "user".into()));
        view.authenticated = true;
        assert!(matches!(
            storage.begin_redemption(&view.secret.as_str().into()),
            Err(RedemptionError::NotApproved)
        ));
        assert!(storage.approve(&view.code.as_str().into(), "user".into()));
        assert!(!storage.approve(&view.code.as_str().into(), "other".into()));
    }

    #[test]
    fn redemption_is_exclusive_cancellation_safe_and_one_time() {
        let storage = QuickConnectStorage::new();
        let view = initiate(&storage);
        let secret = view.secret.as_str().into();
        storage.approve(&view.code.as_str().into(), "user".into());
        let attempt = storage.begin_redemption(&secret).unwrap();
        assert_eq!(attempt.user_id(), "user");
        assert!(matches!(
            storage.begin_redemption(&secret),
            Err(RedemptionError::NotApproved)
        ));
        drop(attempt);
        storage.begin_redemption(&secret).unwrap().complete();
        assert!(storage.poll(&secret).is_none());
        assert!(!storage.approve(&view.code.as_str().into(), "user".into()));
    }

    #[test]
    fn expiration_counts_sessions_once_and_removes_both_indexes() {
        let storage = QuickConnectStorage::new();
        let view = storage.initiate_at(
            "device".into(),
            "TV".into(),
            "App".into(),
            "1".into(),
            Utc::now() - Duration::minutes(11),
        );
        assert_eq!(storage.cleanup_expired(), 1);
        assert!(storage.poll(&view.secret.as_str().into()).is_none());
        assert!(!storage.approve(&view.code.as_str().into(), "user".into()));
        assert_eq!(storage.cleanup_expired(), 0);
    }
}
