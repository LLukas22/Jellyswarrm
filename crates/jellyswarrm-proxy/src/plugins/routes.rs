//! Mount plugins' [`ProxiedRoute`]s, bound to existing proxy handlers, and the
//! routers of plugins that serve an API themselves. The production router
//! merges these ahead of its catch-all, so they inherit the URL prefix,
//! session, read-only key and tracing layers like any core route.
use std::{collections::BTreeMap, sync::Arc};

use axum::{
    routing::{on, MethodFilter, MethodRouter},
    Router,
};
use jellyswarrm_plugin_api::{Behavior, Host, Method, Plugin, ProxiedRoute};

use super::host::ProxyHost;
use crate::{
    application::media_services::MediaServices,
    http::{client_routes, forwarding, state::AppState},
};

/// `settings` holds the `[plugins.<id>]` tables read at startup; plugin
/// secrets are encrypted with keys derived from `session_key`.
pub(crate) fn router(
    plugins: &[&dyn Plugin],
    media: &MediaServices,
    settings: &BTreeMap<String, serde_json::Value>,
    session_key: &[u8],
) -> Router<AppState> {
    let mut router = Router::new();
    for plugin in plugins {
        for route in plugin.proxied_routes() {
            let handler = handler(route);
            for path in route.spellings() {
                tracing::debug!(plugin = plugin.id(), "Registered route: {path}");
                router = router.route(&path, handler.clone());
            }
        }
        let host: Arc<dyn Host> = Arc::new(ProxyHost::new(
            plugin.id(),
            media.clone(),
            settings.get(plugin.id()).cloned(),
            session_key,
        ));
        // A plugin that returns no router logs why itself.
        if let Some(own) = plugin.router(host) {
            tracing::info!("Plugin {} serves its own routes", plugin.id());
            router = router.merge(own.with_state(()));
        }
    }
    router
}

fn handler(route: &ProxiedRoute) -> MethodRouter<AppState> {
    let filter = match route.method {
        Method::Get => MethodFilter::GET,
        Method::Post => MethodFilter::POST,
        Method::Put => MethodFilter::PUT,
        Method::Delete => MethodFilter::DELETE,
    };
    match route.behavior {
        Behavior::Forward => on(filter, forwarding::forward),
        Behavior::ItemList => on(filter, client_routes::item_list),
    }
}
