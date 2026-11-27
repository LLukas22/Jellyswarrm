//! The Jellyfin KodiSyncQueue plugin, used by Jellyfin for Kodi to sync
//! library changes.
//!
//! The real plugin runs on the backends. The catch-all forwarder already
//! routes `/Jellyfin.Plugin.KodiSyncQueue/{userId}/GetItems`; this adapter adds
//! the ID rules: the user ID in the path, and the bare item IDs in the
//! `ItemsAdded`/`ItemsUpdated`/`ItemsRemoved` arrays of the response.
//!
//! Compiled out, the request is still forwarded but with the proxy's user ID
//! and untranslated item IDs, so Kodi's incremental sync stops working.
use jellyswarrm_plugin_api::{Conventions, Plugin};

pub struct KodiSyncQueue;

pub static PLUGIN: KodiSyncQueue = KodiSyncQueue;

const CONVENTIONS: Conventions = Conventions {
    user_id_path_tags: &["Jellyfin.Plugin.KodiSyncQueue"],
    // The plugin returns bare item IDs, not BaseItemDto objects.
    media_id_array_fields: &["ItemsAdded", "ItemsUpdated", "ItemsRemoved"],
    ..Conventions::EMPTY
};

impl Plugin for KodiSyncQueue {
    fn id(&self) -> &'static str {
        "kodi-sync-queue"
    }

    fn name(&self) -> &'static str {
        "KodiSyncQueue"
    }

    fn conventions(&self) -> &'static Conventions {
        &CONVENTIONS
    }
}
