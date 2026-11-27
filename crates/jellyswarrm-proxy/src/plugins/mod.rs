//! Compile-time plugins. Each plugin is a crate behind a Cargo feature, and this
//! registry is the only place those features appear. A plugin that is compiled
//! out contributes no routes and no ID rules, so its requests take the default
//! routing; the core keeps no plugin-specific fallbacks.
mod conventions;
pub(crate) mod routes;

pub(crate) use conventions::conventions;
use jellyswarrm_plugin_api::Plugin;

/// Every compiled-in plugin, in registration order.
pub(crate) static PLUGINS: &[&dyn Plugin] = &[
    #[cfg(feature = "plugin-trickplay")]
    &jellyswarrm_plugin_trickplay::PLUGIN,
    #[cfg(feature = "plugin-intro-skipper")]
    &jellyswarrm_plugin_intro_skipper::PLUGIN,
    #[cfg(feature = "plugin-kodi-sync-queue")]
    &jellyswarrm_plugin_kodi_sync_queue::PLUGIN,
];

/// One startup line listing what this build contains.
pub(crate) fn log_compiled_plugins() {
    if PLUGINS.is_empty() {
        tracing::info!("No plugins compiled in");
    } else {
        let names = PLUGINS
            .iter()
            .map(|plugin| format!("{} ({})", plugin.name(), plugin.id()))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::info!("Compiled-in plugins: {names}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_ids_are_unique_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for plugin in PLUGINS {
            let id = plugin.id();
            assert!(
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "invalid plugin id {id:?}"
            );
            assert!(seen.insert(id), "duplicate plugin id {id:?}");
        }
    }

    /// `ProxiedRoute::spellings` concatenates `nest` and `path`, so only shapes
    /// where that equals Axum's nesting are allowed.
    #[test]
    fn proxied_routes_have_plain_concatenable_paths() {
        for plugin in PLUGINS {
            for route in plugin.proxied_routes() {
                let id = plugin.id();
                assert!(route.path.starts_with('/'), "{id}: {route:?}");
                if let Some(nest) = route.nest {
                    assert!(
                        nest.len() > 1 && nest.starts_with('/') && !nest.ends_with('/'),
                        "{id}: {route:?}"
                    );
                    assert_ne!(route.path, "/", "{id}: {route:?}");
                }
            }
        }
    }
}
