//! Immutable adapter paths resolved at startup, not writable configuration.
#[derive(Clone)]
pub(crate) struct AdapterPaths {
    ui_route: String,
    url_prefix: Option<String>,
}
impl AdapterPaths {
    pub(crate) fn new(startup: &crate::config::StartupConfig) -> Self {
        Self {
            ui_route: startup.ui_route.to_string(),
            url_prefix: startup.url_prefix.as_ref().map(ToString::to_string),
        }
    }
    pub(crate) fn ui_route(&self) -> String {
        match &self.url_prefix {
            Some(prefix) => format!("{prefix}/{}", self.ui_route),
            None => self.ui_route.clone(),
        }
    }
    pub(crate) fn url_prefix(&self) -> Option<String> {
        self.url_prefix.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adapter_paths_are_startup_snapshots_and_preserve_prefix_semantics() {
        let mut config = crate::config::AppConfig {
            url_prefix: Some("proxy".into()),
            ui_route: "manage".into(),
            ..Default::default()
        };
        let paths = AdapterPaths::new(&config.startup_config());
        config.url_prefix = None;
        config.ui_route = "changed".into();
        assert_eq!(paths.ui_route(), "proxy/manage");
        assert_eq!(paths.url_prefix().as_deref(), Some("proxy"));
        let unprefixed = AdapterPaths::new(&config.startup_config());
        assert_eq!(unprefixed.ui_route(), "changed");
    }
}
