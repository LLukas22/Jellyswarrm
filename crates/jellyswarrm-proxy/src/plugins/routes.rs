//! Mount plugins' [`ProxiedRoute`]s, bound to existing proxy handlers. The
//! production router merges these ahead of its catch-all, so they inherit the
//! URL prefix, session, read-only key and tracing layers like any core route.
use axum::{
    routing::{on, MethodFilter, MethodRouter},
    Router,
};
use jellyswarrm_plugin_api::{Behavior, Method, Plugin, ProxiedRoute};

use crate::http::{client_routes, forwarding, state::AppState};

pub(crate) fn router(plugins: &[&dyn Plugin]) -> Router<AppState> {
    let mut router = Router::new();
    for plugin in plugins {
        for route in plugin.proxied_routes() {
            let handler = handler(route);
            for path in route.spellings() {
                tracing::debug!(plugin = plugin.id(), "Registered route: {path}");
                router = router.route(&path, handler.clone());
            }
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
