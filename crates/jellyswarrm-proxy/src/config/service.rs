//! Serialize configuration persistence and publication independently of UI contracts.
use std::sync::Arc;

use tokio::sync::RwLock;

use super::AppConfig;

/// Settings supported by the administrative edit operation. Listener, routing,
/// transport and encryption settings are intentionally not part of this input.
pub(crate) struct SettingsUpdate {
    pub(crate) public_address: String,
    pub(crate) server_name: String,
    pub(crate) include_server_name_in_media: bool,
    pub(crate) auto_create_users_on_login: bool,
    pub(crate) deduplicate_media: bool,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigurationError {
    #[error("all settings fields are required")]
    MissingFields,
    #[error("could not persist configuration")]
    Persistence(#[source] std::io::Error),
    #[error("invalid configuration")]
    Load(#[source] anyhow::Error),
    #[error("session_key cannot be changed while running")]
    SessionKeyChanged,
    #[error("startup configuration changes require a restart")]
    StartupChanged,
    #[error("configuration worker failed")]
    Worker(#[source] tokio::task::JoinError),
}

#[derive(Clone)]
pub(crate) struct ConfigurationService {
    current: Arc<RwLock<AppConfig>>,
    writers: Arc<tokio::sync::Mutex<()>>,
    paths: super::Paths,
}

impl ConfigurationService {
    pub(crate) fn new(current: Arc<RwLock<AppConfig>>, paths: super::Paths) -> Self {
        Self {
            current,
            paths,
            writers: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub(crate) async fn save(&self, update: SettingsUpdate) -> Result<(), ConfigurationError> {
        let paths = self.paths.clone();
        self.save_with(update, move |config| super::save_config(&paths, config))
            .await
    }

    pub(crate) async fn set_library_merging(
        &self,
        enabled: bool,
    ) -> Result<(), ConfigurationError> {
        let paths = self.paths.clone();
        self.persist_update(
            move |next| next.merge_libraries = enabled,
            move |config| super::save_config(&paths, config),
        )
        .await
    }

    async fn save_with(
        &self,
        update: SettingsUpdate,
        persist: impl FnOnce(&AppConfig) -> std::io::Result<()> + Send + 'static,
    ) -> Result<(), ConfigurationError> {
        if update.public_address.trim().is_empty() || update.server_name.trim().is_empty() {
            return Err(ConfigurationError::MissingFields);
        }
        self.persist_update(
            move |next| {
                next.public_address = update.public_address.trim().to_owned();
                next.server_name = update.server_name.trim().to_owned();
                next.include_server_name_in_media = update.include_server_name_in_media;
                next.auto_create_users_on_login = update.auto_create_users_on_login;
                next.deduplicate_media = update.deduplicate_media;
            },
            persist,
        )
        .await
    }

    async fn persist_update(
        &self,
        update: impl FnOnce(&mut AppConfig),
        persist: impl FnOnce(&AppConfig) -> std::io::Result<()> + Send + 'static,
    ) -> Result<(), ConfigurationError> {
        // Serialize writers separately from readers. Once submitted, blocking
        // work owns the writer permit through publication, even if its caller
        // is cancelled: a later update cannot overtake an in-flight disk write.
        let writer = self.writers.clone().lock_owned().await;
        let mut next = self.current.read().await.clone();
        update(&mut next);
        let current = self.current.clone();
        tokio::task::spawn_blocking(move || {
            let _writer = writer;
            persist(&next).map_err(ConfigurationError::Persistence)?;
            *current.blocking_write() = next;
            Ok(())
        })
        .await
        .map_err(ConfigurationError::Worker)?
    }

    pub(crate) async fn reload(&self) -> Result<(), ConfigurationError> {
        let paths = self.paths.clone();
        self.reload_with(move |key| super::load_config_with_session_key(&paths, Some(key)))
            .await
    }

    async fn reload_with(
        &self,
        load: impl FnOnce(&[u8]) -> anyhow::Result<AppConfig> + Send + 'static,
    ) -> Result<(), ConfigurationError> {
        let writer = self.writers.clone().lock_owned().await;
        let previous = self.current.read().await.clone();
        let current = self.current.clone();
        tokio::task::spawn_blocking(move || {
            let _writer = writer;
            let next = load(&previous.session_key).map_err(ConfigurationError::Load)?;
            if previous.session_key != next.session_key {
                return Err(ConfigurationError::SessionKeyChanged);
            }
            if previous.startup_config() != next.startup_config() {
                return Err(ConfigurationError::StartupChanged);
            }
            current
                .blocking_write()
                .publish_runtime_settings(next.runtime_settings());
            Ok(())
        })
        .await
        .map_err(ConfigurationError::Worker)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update() -> SettingsUpdate {
        SettingsUpdate {
            public_address: " https://proxy.example ".into(),
            server_name: " New name ".into(),
            include_server_name_in_media: false,
            auto_create_users_on_login: false,
            deduplicate_media: true,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_slow_save_keeps_readers_live_and_cannot_be_overtaken() {
        let initial = AppConfig {
            merge_libraries: false,
            ..AppConfig::default()
        };
        let current = Arc::new(RwLock::new(initial.clone()));
        let view = crate::config::SettingsView::new(current.clone());
        let service = ConfigurationService::new(current.clone(), super::super::Paths::temporary());
        let persisted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let first = {
            let service = service.clone();
            let persisted = persisted.clone();
            tokio::spawn(async move {
                service
                    .save_with(update(), move |next| {
                        entered.send(()).unwrap();
                        gate.recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                        persisted.lock().unwrap().push(next.clone());
                        Ok(())
                    })
                    .await
            })
        };
        waiting.await.unwrap();
        // This is a single-thread Tokio runtime. Both the snapshot read and
        // subsequent tasks must progress while the filesystem worker is paused.
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(1), view.read())
            .await
            .unwrap();
        assert_eq!(snapshot.server_name, initial.server_name);
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let (queued, queue_seen) = tokio::sync::oneshot::channel();
        let (second_entered, mut second_seen) = tokio::sync::oneshot::channel();
        let second = {
            let service = service.clone();
            let persisted = persisted.clone();
            tokio::spawn(async move {
                queued.send(()).unwrap();
                service
                    .persist_update(
                        |next| next.merge_libraries = true,
                        move |next| {
                            second_entered.send(()).unwrap();
                            persisted.lock().unwrap().push(next.clone());
                            Ok(())
                        },
                    )
                    .await
            })
        };
        queue_seen.await.unwrap();
        tokio::task::yield_now().await;
        assert!(matches!(
            second_seen.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(view.read().await.server_name, initial.server_name);
        release.send(()).unwrap();
        second.await.unwrap().unwrap();
        second_seen.await.unwrap();
        {
            let snapshots = persisted.lock().unwrap();
            assert_eq!(snapshots.len(), 2);
            assert_eq!(snapshots[0].server_name, "New name");
            assert!(!snapshots[0].merge_libraries);
            assert_eq!(snapshots[1].server_name, "New name");
            assert!(snapshots[1].merge_libraries);
        }
        assert!(current.read().await.merge_libraries);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_slow_reload_keeps_readers_live_and_serializes_with_save() {
        let initial = AppConfig::default();
        let current = Arc::new(RwLock::new(initial.clone()));
        let service = ConfigurationService::new(current.clone(), super::super::Paths::temporary());
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let reload = {
            let service = service.clone();
            let mut loaded = initial.clone();
            loaded.server_name = "Reloaded".into();
            tokio::spawn(async move {
                service
                    .reload_with(move |_| {
                        entered.send(()).unwrap();
                        gate.recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                        Ok(loaded)
                    })
                    .await
            })
        };
        waiting.await.unwrap();
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(1), current.read())
            .await
            .unwrap();
        assert_eq!(snapshot.server_name, initial.server_name);
        drop(snapshot);
        reload.abort();
        assert!(reload.await.unwrap_err().is_cancelled());
        let (queued, queue_seen) = tokio::sync::oneshot::channel();
        let save = tokio::spawn(async move {
            queued.send(()).unwrap();
            service
                .persist_update(
                    |next| next.deduplicate_media = true,
                    |next| {
                        assert_eq!(next.server_name, "Reloaded");
                        assert!(next.deduplicate_media);
                        Ok(())
                    },
                )
                .await
        });
        queue_seen.await.unwrap();
        tokio::task::yield_now().await;
        assert!(!save.is_finished());
        release.send(()).unwrap();
        save.await.unwrap().unwrap();
        assert_eq!(current.read().await.server_name, "Reloaded");
        assert!(current.read().await.deduplicate_media);
    }

    #[tokio::test]
    async fn failed_save_does_not_publish_and_success_preserves_startup_settings() {
        let initial = AppConfig::default();
        let current = Arc::new(RwLock::new(initial.clone()));
        let service = ConfigurationService::new(current.clone(), super::super::Paths::temporary());
        let result = service
            .save_with(update(), |_| Err(std::io::Error::other("disk full")))
            .await;
        assert!(matches!(result, Err(ConfigurationError::Persistence(_))));
        assert_eq!(current.read().await.server_name, initial.server_name);
        service
            .save_with(update(), |saved| {
                assert_eq!(saved.server_name, "New name");
                assert_eq!(saved.public_address, "https://proxy.example");
                Ok(())
            })
            .await
            .unwrap();
        let saved = current.read().await;
        assert_eq!(saved.server_name, "New name");
        assert_eq!(saved.session_key, initial.session_key);
        assert_eq!(saved.port, initial.port);
        assert!(saved.deduplicate_media);
    }

    #[tokio::test]
    async fn invalid_update_does_not_attempt_persistence() {
        let service = ConfigurationService::new(
            Arc::new(RwLock::new(AppConfig::default())),
            super::super::Paths::temporary(),
        );
        let mut invalid = update();
        invalid.server_name = "  ".into();
        assert!(matches!(
            service
                .save_with(invalid, |_| panic!("must not persist"))
                .await,
            Err(ConfigurationError::MissingFields)
        ));
    }

    #[tokio::test]
    async fn reload_rejects_startup_changes_without_partially_publishing_runtime_settings() {
        let initial = AppConfig::default();
        let current = Arc::new(RwLock::new(initial.clone()));
        let service = ConfigurationService::new(current.clone(), super::super::Paths::temporary());
        let proposed = initial.clone();
        let result = service
            .reload_with(move |_| {
                let mut next = proposed;
                next.port += 1;
                next.server_name = "Must not publish".into();
                Ok(next)
            })
            .await;
        assert!(matches!(result, Err(ConfigurationError::StartupChanged)));
        assert_eq!(current.read().await.server_name, initial.server_name);
        assert_eq!(current.read().await.port, initial.port);
    }

    #[tokio::test]
    async fn failed_reload_and_key_rotation_preserve_the_active_snapshot() {
        let initial = AppConfig::default();
        let current = Arc::new(RwLock::new(initial.clone()));
        let service = ConfigurationService::new(current.clone(), super::super::Paths::temporary());
        assert!(matches!(
            service.reload_with(|_| anyhow::bail!("invalid file")).await,
            Err(ConfigurationError::Load(_))
        ));
        assert!(matches!(
            service.reload_with(|_| Ok(AppConfig::default())).await,
            Err(ConfigurationError::SessionKeyChanged)
        ));
        assert_eq!(current.read().await.session_key, initial.session_key);
        service
            .reload_with(move |key| {
                let mut next = initial.clone();
                assert_eq!(key, initial.session_key);
                next.server_name = "Reloaded".into();
                Ok(next)
            })
            .await
            .unwrap();
        assert_eq!(current.read().await.server_name, "Reloaded");
    }
}
