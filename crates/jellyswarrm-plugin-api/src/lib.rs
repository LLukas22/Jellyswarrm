//! Contract between Jellyswarrm and its compile-time plugins.
//!
//! A plugin is a crate behind a Cargo feature of `jellyswarrm-proxy`. When the
//! feature is off the plugin contributes nothing: its routes are not registered
//! and its ID rules are not applied, so its requests take the proxy's default
//! routing. The proxy never contains plugin-specific behaviour of its own.
//!
//! Adapter plugins, where the real plugin or Jellyfin feature runs on the
//! backends, are plain data: [`Conventions`] tell the translation layer where
//! media and user IDs appear, and [`ProxiedRoute`]s bind paths to existing
//! proxy behaviour. Plugins that implement an API themselves return their own
//! [`Plugin::router`] and reach the proxy only through [`Host`].
//!
//! A plugin can bind its state before returning its router. The [`Caller`]
//! extractor uses the host to authenticate each request, and
//! `host.settings_as::<T>()` reads the plugin's configuration. Tests build
//! the router on `testing::TestHost` (feature `testing`).
//!
//! ```
//! use std::sync::Arc;
//! use axum::{routing::get, Router};
//! use jellyswarrm_plugin_api::{Caller, Host, Plugin};
//!
//! pub struct Example;
//! pub static PLUGIN: Example = Example;
//!
//! impl Plugin for Example {
//!     fn id(&self) -> &'static str { "example" }
//!     fn name(&self) -> &'static str { "Example" }
//!
//!     fn router(&self, host: Arc<dyn Host>) -> Option<Router> {
//!         let router = Router::new()
//!             .route("/Example/Me", get(|caller: Caller| async { caller.user_name }))
//!             .with_state(host);
//!         Some(router)
//!     }
//! }
//! ```
mod host;
#[cfg(feature = "testing")]
pub mod testing;

pub use host::{Caller, CredentialKind, Host, HostError, SecretRevision, UserSecret};
use std::sync::Arc;

/// A compiled-in plugin. Implementations are unit structs exported as a
/// `pub static PLUGIN`, so the proxy's registry is a static slice.
pub trait Plugin: Send + Sync + 'static {
    /// Stable identifier: lowercase ASCII letters, digits and `-`.
    fn id(&self) -> &'static str;

    /// Human-readable name for logs.
    fn name(&self) -> &'static str;

    /// ID rules merged into the proxy's translation layer. Static so the proxy
    /// can build its tables before any services exist.
    fn conventions(&self) -> &'static Conventions {
        &Conventions::EMPTY
    }

    /// Routes bound to existing proxy behaviour.
    fn proxied_routes(&self) -> &'static [ProxiedRoute] {
        &[]
    }

    /// The plugin's own handlers, built once at startup. Return `None` to skip
    /// these handlers. The proxy registers [`Self::proxied_routes`] and
    /// [`Self::conventions`] whenever the plugin is compiled in.
    fn router(&self, _host: Arc<dyn Host>) -> Option<axum::Router> {
        None
    }
}

/// Where a plugin's media and user IDs appear in requests and responses.
/// Every entry is matched case-insensitively and merged with the proxy's own
/// lists; entries the proxy already has are harmless but redundant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conventions {
    /// Path segments followed by a media ID, e.g. `"MediaSegments"` for
    /// `/MediaSegments/{id}`. Also used to pick the backend that owns the ID.
    pub media_id_path_tags: &'static [&'static str],
    /// Path segments followed by a user ID, e.g. `"Users"` for `/Users/{id}`.
    pub user_id_path_tags: &'static [&'static str],
    /// Response arrays whose string items are bare media IDs.
    pub media_id_array_fields: &'static [&'static str],
    /// Response objects whose keys are media IDs.
    pub media_id_map_key_fields: &'static [&'static str],
}

impl Conventions {
    pub const EMPTY: Self = Self {
        media_id_path_tags: &[],
        user_id_path_tags: &[],
        media_id_array_fields: &[],
        media_id_map_key_fields: &[],
    };
}

impl Default for Conventions {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// HTTP method of a [`ProxiedRoute`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

/// A route the proxy registers ahead of its catch-all and serves with an
/// existing [`Behavior`].
///
/// Declare each route once in its canonical spelling. The proxy registers the
/// same spellings its own routes accept: `nest` and `path` each as written and
/// lowercased, so a nested route matches four ways and a flat one two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxiedRoute {
    pub method: Method,
    /// Optional prefix, e.g. `Some("/Videos")`. Starts with `/` and does not
    /// end with one; the proxy concatenates it with `path`.
    pub nest: Option<&'static str>,
    /// Axum path syntax relative to `nest`, e.g. `"/{stream_id}/Trickplay/{*path}"`.
    /// Starts with `/`, and is not `/` alone when `nest` is set.
    pub path: &'static str,
    pub behavior: Behavior,
}

/// Existing proxy behaviour a [`ProxiedRoute`] delegates to. Grows only when an
/// adapter needs another one; the proxy must map every variant to a handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// Same as the catch-all: pick the backend that owns the request's IDs,
    /// translate the request and the response, and stream the body back.
    Forward,
    /// An item-scoped list from the owning backend, without merging media
    /// versions (the proxy's handler for `/Items/{id}/Similar` and friends).
    ItemList,
}

impl ProxiedRoute {
    /// Every path spelling the proxy registers for this route, deduplicated.
    pub fn spellings(&self) -> Vec<String> {
        let variants = |value: &str| {
            let lower = value.to_lowercase();
            if lower == value {
                vec![value.to_owned()]
            } else {
                vec![value.to_owned(), lower]
            }
        };
        let prefixes = self.nest.map_or_else(|| vec![String::new()], variants);
        let paths = variants(self.path);
        prefixes
            .iter()
            .flat_map(|prefix| paths.iter().map(move |path| format!("{prefix}{path}")))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(nest: Option<&'static str>, path: &'static str) -> ProxiedRoute {
        ProxiedRoute {
            method: Method::Get,
            nest,
            path,
            behavior: Behavior::Forward,
        }
    }

    #[test]
    fn nested_routes_match_the_four_spellings_of_lowercase_routes() {
        assert_eq!(
            route(Some("/Videos"), "/{stream_id}/Trickplay/{*path}").spellings(),
            [
                "/Videos/{stream_id}/Trickplay/{*path}",
                "/Videos/{stream_id}/trickplay/{*path}",
                "/videos/{stream_id}/Trickplay/{*path}",
                "/videos/{stream_id}/trickplay/{*path}",
            ]
        );
    }

    #[test]
    fn flat_and_lowercase_routes_are_not_duplicated() {
        assert_eq!(
            route(None, "/MediaSegments/{item_id}").spellings(),
            ["/MediaSegments/{item_id}", "/mediasegments/{item_id}"]
        );
        assert_eq!(
            route(Some("/videos"), "/{id}/x").spellings(),
            ["/videos/{id}/x"]
        );
    }
}
