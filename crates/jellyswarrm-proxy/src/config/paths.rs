//! Filesystem locations resolved once by startup, then shared explicitly.
use std::path::PathBuf;

#[derive(Clone)]
pub(crate) struct Paths {
    pub(crate) data: PathBuf,
    pub(crate) logs: PathBuf,
    pub(crate) db: PathBuf,
    pub(crate) config: PathBuf,
    pub(crate) dev_config: PathBuf,
    #[cfg(test)]
    _temporary: Option<std::sync::Arc<tempfile::TempDir>>,
}

impl Paths {
    pub(crate) fn new(data: PathBuf) -> Self {
        Self {
            logs: data.join("logs"),
            db: data.join("jellyswarrm.db"),
            config: data.join(super::DEFAULT_CONFIG_FILENAME),
            dev_config: data.join("jellyswarrm.dev.toml"),
            data,
            #[cfg(test)]
            _temporary: None,
        }
    }

    pub(crate) fn resolve() -> std::io::Result<Self> {
        let data = match std::env::var("JELLYSWARRM_DATA_DIR") {
            Ok(data) => PathBuf::from(data),
            Err(_) => std::env::current_dir()?.join("data"),
        };
        let data = if data.is_absolute() {
            data
        } else {
            std::env::current_dir()?.join(data)
        };
        let paths = Self::new(data);
        std::fs::create_dir_all(&paths.data)?;
        Ok(paths)
    }

    #[cfg(test)]
    pub(crate) fn temporary() -> Self {
        let dir = std::sync::Arc::new(tempfile::tempdir().unwrap());
        let mut paths = Self::new(dir.path().to_owned());
        paths._temporary = Some(dir);
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_paths_preserve_keys_and_configuration() {
        let first = Paths::temporary();
        let second = Paths::temporary();
        let a = super::super::AppConfig {
            server_name: "first instance".into(),
            ..Default::default()
        };
        let b = super::super::AppConfig::default();
        super::super::save_config(&first, &a).unwrap();
        super::super::save_config(&second, &b).unwrap();
        let read = |p: &Paths| -> super::super::AppConfig {
            toml::from_str(&std::fs::read_to_string(&p.config).unwrap()).unwrap()
        };
        assert_eq!(read(&first).server_name, "first instance");
        assert_eq!(read(&first).session_key, a.session_key);
        assert_eq!(read(&second).session_key, b.session_key);
        assert_ne!(first.db, second.db);
        let clone = first.clone();
        drop(first);
        assert!(clone.config.exists());
    }
}
