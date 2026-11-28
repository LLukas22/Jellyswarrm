# Plugins

Some Jellyfin features come from server plugins, or from Jellyfin APIs that
need translation Jellyswarrm doesn't do for every route. Jellyswarrm supports
them through **compile-time plugins**: one crate per plugin under
[`crates/plugins/`](../crates/plugins), each behind a Cargo feature of
`jellyswarrm-proxy`.

All plugins are enabled by default, and the official Docker image includes
them. Compiling a plugin out removes its handlers, proxied routes and ID
conventions. Its requests then take Jellyswarrm's normal routing.

| Plugin | Feature | Configuration | What it does |
| --- | --- | --- | --- |
| [Trickplay](#trickplay) | `plugin-trickplay` | none | Scrubbing previews on the seek bar |
| [Intro Skipper](#intro-skipper) | `plugin-intro-skipper` | none | Skip intro/outro buttons |
| [KodiSyncQueue](#kodisyncqueue) | `plugin-kodi-sync-queue` | none | Incremental library sync for Jellyfin for Kodi |
| [Moonfin](#moonfin) | `plugin-moonfin` | `[plugins.moonfin]` | Moonfin's Seerr integration |

Plugins read their settings from a `[plugins.<id>]` table in
`jellyswarrm.toml` at startup, so changes require a restart. Environment
variables reach them with a double underscore, e.g.
`JELLYSWARRM_PLUGINS__MOONFIN__SEERR_URL`.

## Trickplay

Jellyfin 10.9+ generates trickplay tiles, and items list them in a `Trickplay`
object keyed by media source ID. The plugin translates those keys and forwards
`/Videos/{id}/Trickplay/…` tile requests to the server that owns the item.

Compiled out: tile requests reach the generic video route and the keys keep
backend IDs, so previews stop working.

## Intro Skipper

The [Intro Skipper](https://github.com/intro-skipper/intro-skipper) plugin, and
any other segment provider, publishes intros, outros and recaps through
Jellyfin's native `/MediaSegments/{id}` API (Jellyfin 10.10+). The plugin sends
that request to the server that owns the item and translates the IDs in the
answer. Install Intro Skipper on the backends as usual.

Compiled out: the request reaches a backend with an ID it doesn't know, so no
skip buttons appear.

## KodiSyncQueue

[Jellyfin for Kodi](https://github.com/jellyfin/jellyfin-kodi) uses the
KodiSyncQueue backend plugin to sync only what changed. The plugin translates
the user ID in `/Jellyfin.Plugin.KodiSyncQueue/{userId}/GetItems` and the item
IDs in its response. Install KodiSyncQueue on the backends as usual.

Compiled out: the request reaches a backend untranslated, so incremental sync
stops working.

## Moonfin

[Moonfin](https://github.com/Moonfin-Client) clients rely on a companion server
plugin. Its data is per user, so Jellyswarrm serves the parts below itself
rather than forwarding them to one backend:

| Endpoint | |
| --- | --- |
| `GET /Moonfin/Ping` | Tells clients the plugin is installed and which features it offers |
| `GET /Moonfin/Discovery` | The server address, for Moonfin Web |
| `/Moonfin/Seerr/*` (and the legacy `/Moonfin/Jellyseerr/*`) | Seerr sign-in, status and request forwarding |

Other Moonfin features (settings sync, media bar, games, …) aren't served and
take normal routing.

```toml
[plugins.moonfin]
seerr_url = "http://seerr:5055"     # as reachable from Jellyswarrm, with any sub-path
seerr_display_name = "Requests"      # optional, default "Seerr"
```

Use an HTTP or HTTPS base URL with an optional path. Credentials, query strings and fragments are rejected. Ping and Config expose this address to signed-in clients.

Without `seerr_url`, Moonfin is not mounted and `/Moonfin/*` goes to a backend
as before.

Seerr must use **Jellyswarrm as its Jellyfin server**: users sign in to Seerr
from Moonfin with their Jellyswarrm credentials. Jellyswarrm keeps each user's
Seerr session encrypted in its database and deletes it with the user. A session
is tied to the configured Seerr URL; after changing it, users sign in to Seerr
again. Quick Connect sign-in to Seerr is not supported. Read-only API keys can
read Ping and Config. Seerr sign-in, status, validation, logout and API requests
require a Jellyswarrm user login.

## Writing a plugin

A plugin is a crate under `crates/plugins/<name>` that depends on
[`jellyswarrm-plugin-api`](../crates/jellyswarrm-plugin-api/src/lib.rs) and
exports a `pub static PLUGIN` implementing `Plugin`. The API documentation
(`cargo doc -p jellyswarrm-plugin-api --open`) has compiled examples.

### Building blocks

A plugin uses any combination of three things:

- **Conventions** (`Plugin::conventions`): where media and user IDs appear,
  e.g. a path segment followed by an item ID, or a response array of item
  IDs. The proxy then translates them like its own. Use these when the real
  feature runs on the backends.
- **Proxied routes** (`Plugin::proxied_routes`): paths the proxy must serve
  with an existing behaviour, such as forwarding to the backend that owns the
  item. Declare each path once; the proxy registers the same case variants as
  its own routes. See [trickplay](../crates/plugins/trickplay/src/lib.rs) and
  [intro-skipper](../crates/plugins/intro-skipper/src/lib.rs).
- **Its own router** (`Plugin::router`): an Axum router with handlers written
  by the plugin, for APIs Jellyswarrm has to answer itself. See
  [moonfin](../crates/plugins/moonfin/src/lib.rs). Return `None` to skip these
  handlers when configuration is missing. The proxy registers proxied routes
  and ID conventions for every compiled-in plugin, including plugins that
  return `None`.

A plugin's own handlers reach Jellyswarrm only through the `Host` passed to
`router`:

| `Host` | |
| --- | --- |
| `Caller` extractor | The authenticated user and the kind of credential used |
| `settings_as::<T>()` | The `[plugins.<id>]` table, deserialized into your type |
| `server_name()`, `url_prefix()` | For building responses and addresses |
| `user_secret`, `set_user_secret`, `replace_user_secret`, `take_user_secret` | Encrypted values per user, deleted with the user |

### Settings

Define a `serde::Deserialize` struct with `#[serde(deny_unknown_fields)]` so a
misspelled key is reported instead of ignored, and read it in `router`. Log
and return `None` when it's missing or invalid. Settings are read once at
startup.

### Secrets

Values are strings, scoped to your plugin, the user and a key you choose;
store structured data as JSON. `set_user_secret` writes unconditionally, e.g.
after a successful login. To update or remove a value after something slow,
such as a request to another service, pass the `revision` from `user_secret`
to `replace_user_secret`. It returns `false` if the value changed in the
meantime, so stale work never overwrites a newer write. For logout, use
`take_user_secret` to atomically remove the session and return the credential
to revoke upstream.

### Rules

- Plugin routes are merged with the proxy's; a path that clashes with
  another route fails at startup. Keep routes under your plugin's own prefix.
- The proxy limits read-only API keys to `GET` and `HEAD`. Require
  `caller.credential == CredentialKind::Login` in handlers that use a user's
  external credentials, including read handlers.
- Never log secrets, credentials or configured URLs, which can contain
  credentials.

### Testing

Enable the API's `testing` feature in your dev-dependencies and build your
router on `testing::TestHost`, an in-memory host with settings, signed-in
users and secrets that behave like the proxy's:

```toml
[dev-dependencies]
jellyswarrm-plugin-api = { workspace = true, features = ["testing"] }
```

Test the wiring through the real proxy in
`crates/jellyswarrm-proxy/src/http_tests/`, behind your feature.

### Registering the plugin

1. Add the crate to `[workspace.dependencies]` in the root `Cargo.toml`.
2. In `crates/jellyswarrm-proxy/Cargo.toml`, add it as an optional dependency,
   add a `plugin-<id>` feature for it, and include the feature in `default`.
3. List `PLUGIN` behind that feature in
   `crates/jellyswarrm-proxy/src/plugins/mod.rs`.
4. Check both builds: `cargo test -p jellyswarrm-proxy --no-default-features
   --features plugin-<id>` and `just test-no-plugins`.
