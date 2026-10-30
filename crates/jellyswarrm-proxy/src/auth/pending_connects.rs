//! Backend approvals owned by one server instance. Removal atomically claims redemption.
use crate::{domain::ids::ServerId, domain::user_id::UserId};
use moka::future::Cache;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(crate) struct PendingConnect {
    pub(crate) user_id: UserId,
    pub(crate) server_id: ServerId,
    pub(crate) secret: String,
}

#[derive(Clone)]
pub(crate) struct PendingConnects {
    cache: Cache<String, (PendingConnect, Instant)>,
    ttl: Duration,
}

impl Default for PendingConnects {
    fn default() -> Self {
        Self::with_ttl(Duration::from_secs(600))
    }
}

impl PendingConnects {
    fn with_ttl(ttl: Duration) -> Self {
        Self {
            cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(ttl)
                .build(),
            ttl,
        }
    }

    pub(crate) async fn insert(&self, id: String, approval: PendingConnect) {
        self.cache
            .insert(id, (approval, Instant::now() + self.ttl))
            .await;
    }

    pub(crate) async fn get(&self, id: &str, user: &UserId, server: ServerId) -> Option<String> {
        self.cache
            .get(id)
            .await
            .filter(|(p, expires)| {
                &p.user_id == user && p.server_id == server && Instant::now() < *expires
            })
            .map(|(p, _)| p.secret)
    }

    pub(crate) async fn claim(&self, id: &str) -> bool {
        // Moka removal can return an expired entry awaiting maintenance. Check
        // its deadline after the atomic removal, including expiry during polling.
        self.cache
            .remove(id)
            .await
            .is_some_and(|(_, expires)| Instant::now() < expires)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn expired_approvals_cannot_be_read_or_redeemed() {
        let pending = PendingConnects::with_ttl(Duration::ZERO);
        let user = UserId::new("viewer");
        let server = ServerId::new(1);
        pending
            .insert(
                "expired".into(),
                PendingConnect {
                    user_id: user.clone(),
                    server_id: server,
                    secret: "secret".into(),
                },
            )
            .await;
        assert!(pending.get("expired", &user, server).await.is_none());
        assert!(!pending.claim("expired").await);
    }

    #[tokio::test]
    async fn ownership_is_scoped_and_redemption_is_atomic() {
        let pending = PendingConnects::default();
        let other = PendingConnects::default();
        let user = UserId::new("viewer");
        let server = ServerId::new(1);
        pending
            .insert(
                "request".into(),
                PendingConnect {
                    user_id: user.clone(),
                    server_id: server,
                    secret: "secret".into(),
                },
            )
            .await;
        assert!(other.get("request", &user, server).await.is_none());
        assert!(pending
            .get("request", &UserId::new("other"), server)
            .await
            .is_none());
        assert!(pending
            .get("request", &user, ServerId::new(2))
            .await
            .is_none());
        assert_eq!(
            pending.get("request", &user, server).await.as_deref(),
            Some("secret")
        );
        let (first, second) = tokio::join!(pending.claim("request"), pending.claim("request"));
        assert_ne!(first, second);
        assert!(pending.get("request", &user, server).await.is_none());
    }
}
