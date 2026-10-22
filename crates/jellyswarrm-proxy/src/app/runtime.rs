//! Own permanent process tasks. Feature constructors never start these tasks;
//! dropping the runtime also aborts them on startup/serve errors.
use tokio::task::JoinSet;
use tower_sessions::ExpiredDeletion;
use tower_sessions_sqlx_store::SqliteStore;

use crate::AppState;

pub(super) struct ApplicationRuntime {
    tasks: JoinSet<()>,
}

impl ApplicationRuntime {
    pub(super) fn start(state: &AppState, store: SqliteStore, health_interval: u64) -> Self {
        let mut tasks = JoinSet::new();
        let servers = state.server_storage.clone();
        tasks.spawn(async move {
            servers.health_check_loop(health_interval).await;
        });
        let quick_connect = state.quick_connect.clone();
        tasks.spawn(async move {
            quick_connect.cleanup_loop().await;
        });
        tasks.spawn(async move {
            if let Err(error) = store
                .continuously_delete_expired(std::time::Duration::from_secs(60))
                .await
            {
                tracing::error!(%error, "Expired HTTP session cleanup stopped");
            }
        });
        Self { tasks }
    }

    pub(super) async fn shutdown(mut self) {
        self.tasks.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    struct OnDrop(Arc<AtomicBool>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn shutdown_cancels_and_joins_owned_work() {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = OnDrop(dropped.clone());
        let mut tasks = JoinSet::new();
        let (started, ready) = tokio::sync::oneshot::channel();
        tasks.spawn(async move {
            let _guard = guard;
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        ApplicationRuntime { tasks }.shutdown().await;
        assert!(dropped.load(Ordering::SeqCst));
    }
}
