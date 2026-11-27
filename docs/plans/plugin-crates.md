# Plan: plugin crates

Status: PR 1 and PR 2 implemented (uncommitted) on `plugin-support`, 2026-11-27. PR 3 (host services, Moonfin Ping/Discovery) is next.
Related issues: #195 (Moonfin), #244 (web UI injection), #225 (plugin manager).

## 1. Goal

Move plugin-specific behaviour out of the core proxy into one crate per plugin, each behind a Cargo feature. The core should know nothing about any particular plugin, only how to merge what plugins contribute.

### Decisions already made

| Decision | Detail |
|---|---|
| One crate per plugin | `crates/plugins/<name>`, package `jellyswarrm-plugin-<name>` |
| Compile-time gating only | No runtime on/off switches, no plugin manager UI |
| All plugins on by default | Every plugin feature, Moonfin included, is in `default`, so the official Docker image ships them all |
| Disabled means absent | A compiled-out plugin registers no routes and no ID rules. Its requests fall back to the proxy's default routing. The core has no plugin-specific exceptions. |
| Unconfigured means absent | A plugin may decline to mount its router when its config is missing (Moonfin with no Seerr URL). Same fall-through as compiled out. Config changes need a restart. |

### Non-goals

- Loading third-party plugins at runtime (DLL, WASM, sidecars). The `Host` trait below is the contract we could later expose to those; it is not part of this work.
- Web UI injection (#244). Noted as a future `Plugin` hook in §9.
- Moonfin features beyond Ping, Discovery and Seerr in the first Moonfin PR.

## 2. Initial plugins

| Crate | Feature | Kind | Owns |
|---|---|---|---|
| `plugins/trickplay` | `plugin-trickplay` | adapter | `GET /Videos/{id}/Trickplay/{*path}` (forward), `Trickplay` map-key rewrite in responses |
| `plugins/intro-skipper` | `plugin-intro-skipper` | adapter | `GET /MediaSegments/{id}` (item-scoped list), `MediaSegments` media-ID path tag |
| `plugins/kodi-sync-queue` | `plugin-kodi-sync-queue` | adapter | `Jellyfin.Plugin.KodiSyncQueue` user-ID path tag, `ItemsAdded`/`ItemsUpdated`/`ItemsRemoved` ID-array fields |
| `plugins/moonfin` | `plugin-moonfin` | emulated | `/Moonfin/Ping`, `/Moonfin/Discovery[/discover]`, `/Moonfin/Seerr/*` (+ `/Moonfin/Jellyseerr/*` alias) |

- **Adapter:** the real plugin, or the native Jellyfin feature, runs on the backends. Jellyswarrm only routes the request and translates IDs. Adapters are pure data: ID rules plus routes bound to existing core behaviour.
- **Emulated:** Jellyswarrm implements the API itself, because forwarding to one backend would be wrong (per-backend users and data). Emulated plugins ship their own axum handlers.

Trickplay and media segments are native Jellyfin APIs (10.9 / 10.10+). The bundled web client calls them through the SDK (`ui/src/apps/legacy/features/playback/utils/mediaSegmentManager.ts:4`). They are plugins here so the core stays generic, and they stay on by default.

## 3. Workspace layout

```
crates/
  jellyswarrm-plugin-api/          # traits + data types; no dependency on jellyswarrm-proxy
    src/lib.rs                     # Plugin, Conventions, ProxiedRoute, Behavior
    src/host.rs                    # Host, PluginStore, Scope, HostError
    src/caller.rs                  # Caller + axum extractor
  plugins/
    trickplay/
    intro-skipper/
    kodi-sync-queue/
    moonfin/
  jellyswarrm-proxy/
    src/plugins/
      mod.rs                       # registry: the only #[cfg(feature)] list
      conventions.rs               # core lists + merged ActiveConventions
      routes.rs                    # mounts ProxiedRoutes and plugin routers
      host.rs                      # ProxyHost: impl Host over MediaServices etc.
      store.rs                     # sqlite PluginStore
```

Root `Cargo.toml`:

```toml
members = ["crates/jellyswarrm-proxy", "crates/jellyswarrm-macros", "crates/jellyfin-api",
           "crates/jellyswarrm-plugin-api", "crates/plugins/*"]

[workspace.dependencies]
jellyswarrm-plugin-api = { path = "crates/jellyswarrm-plugin-api" }
jellyswarrm-plugin-trickplay = { path = "crates/plugins/trickplay" }
# ... one line per plugin
```

`crates/jellyswarrm-proxy/Cargo.toml`:

```toml
[features]
default = ["plugin-trickplay", "plugin-intro-skipper", "plugin-kodi-sync-queue", "plugin-moonfin"]
plugin-trickplay = ["dep:jellyswarrm-plugin-trickplay"]
plugin-intro-skipper = ["dep:jellyswarrm-plugin-intro-skipper"]
plugin-kodi-sync-queue = ["dep:jellyswarrm-plugin-kodi-sync-queue"]
plugin-moonfin = ["dep:jellyswarrm-plugin-moonfin"]
browser-tests = ["dep:playwright-rs"]

[dependencies]
jellyswarrm-plugin-api = { workspace = true }
jellyswarrm-plugin-trickplay = { workspace = true, optional = true }
# ...
```

The Dockerfile builds `--bin jellyswarrm-proxy` with default features, so the official image includes every plugin with no Dockerfile change. The `cargo chef` recipe picks up the new path crates automatically.

## 4. `jellyswarrm-plugin-api`

PR 1 needs only route descriptors and the HTTP method type. Add axum and the caller dependencies in PR 3. Add the outbound client and secret store in PR 4. The trait below shows the intended shape after PR 3.

### 4.1 Plugin trait

```rust
pub trait Plugin: Send + Sync + 'static {
    /// Stable id: config table name, storage namespace, log target. e.g. "moonfin".
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;

    /// ID rules merged into the proxy's translation layer. Must be static so the
    /// proxy can build its tables before any services exist.
    fn conventions(&self) -> &'static Conventions { &Conventions::EMPTY }

    /// Routes bound to existing proxy behaviour (adapters).
    fn proxied_routes(&self) -> &'static [ProxiedRoute] { &[] }

    /// Added in PR 3. Adapters use None. `None` also means "not configured":
    /// nothing is mounted and requests fall through (Moonfin without a Seerr URL).
    fn router(&self, _host: Arc<dyn Host>) -> Option<axum::Router<Arc<dyn Host>>> { None }
}
```

Plugins are unit structs exported as statics (`pub static PLUGIN: Trickplay = Trickplay;`), so the registry is a `&'static [&'static dyn Plugin]`.

### 4.2 Conventions

All fields are `&'static [&'static str]`, so a plugin defines them in a `const`:

```rust
pub struct Conventions {
    pub media_id_path_tags: &'static [&'static str],      // -> MEDIA_ID_PATH_TAGS
    pub user_id_path_tags: &'static [&'static str],       // -> USER_ID_PATH_TAGS
    pub media_id_array_fields: &'static [&'static str],   // -> MEDIA_ID_ARRAY_FIELDS
    pub media_id_map_key_fields: &'static [&'static str], // -> MEDIA_ID_MAP_KEY_FIELDS
}
```

Only the lists some plugin actually needs are included. More can be added when a plugin needs them; `ID_FIELDS`, `SESSION_FIELDS` etc. stay core-only.

### 4.3 Proxied routes

```rust
pub struct ProxiedRoute {
    pub method: Method,              // GET/POST/...
    pub nest: Option<&'static str>,  // e.g. Some("/Videos")
    pub path: &'static str,          // axum syntax, relative to `nest`, e.g. "/{stream_id}/Trickplay/{*path}"
    pub behavior: Behavior,
}

pub enum Behavior {
    /// Same as the catch-all: pick the owning backend from IDs, translate both ways.
    Forward,
    /// Item-scoped list without version merging (`client_routes::item_list`).
    ItemList,
}
```

The enum grows only when an adapter needs another core behaviour. Each variant maps to an existing handler in `http/client_routes` / `http/forwarding`.

Plugins declare each route once, in its canonical spelling. The core expands the spellings the same way `lowercase_routes!` does for literals: the nest prefix and the path each appear as written and lowercased. A route with a nest therefore gets four spellings (§5.3), one without gets two.

### 4.4 Host

The only way a plugin reaches Jellyswarrm. One instance per plugin, already scoped to that plugin's id.

```rust
#[async_trait]
pub trait Host: Send + Sync + 'static {
    fn server_name(&self) -> String;
    fn server_id(&self) -> String;
    fn url_prefix(&self) -> Option<String>;
    fn http_client(&self) -> reqwest::Client;     // shared client, proxy timeout
    /// `[plugins.<id>]` from jellyswarrm.toml as JSON, read at startup.
    fn settings(&self) -> Option<serde_json::Value>;
    /// Resolve the caller from headers/URI with the proxy's normal token rules.
    async fn caller(&self, parts: &http::request::Parts) -> Result<Option<Caller>, HostError>;
    fn store(&self) -> &dyn PluginStore;
}

#[async_trait]
pub trait PluginStore: Send + Sync {
    async fn get(&self, scope: Scope<'_>, key: &str) -> Result<Option<Value>, HostError>;
    async fn put(&self, scope: Scope<'_>, key: &str, value: &Value) -> Result<(), HostError>;
    async fn delete(&self, scope: Scope<'_>, key: &str) -> Result<(), HostError>;
    /// Write only if the stored value still equals `expected` (None = absent). Returns false on mismatch.
    async fn compare_and_put(&self, scope: Scope<'_>, key: &str, expected: Option<&Value>, value: &Value) -> Result<bool, HostError>;
    /// Encrypted at rest with a per-plugin key.
    async fn get_secret(&self, scope: Scope<'_>, key: &str) -> Result<Option<String>, HostError>;
    async fn put_secret(&self, scope: Scope<'_>, key: &str, value: &str) -> Result<(), HostError>;
}

pub enum Scope<'a> { Global, User(&'a str) }   // User = virtual (proxy) user id
```

`reqwest` becomes a dependency of the API crate (workspace version, same features). That's acceptable; the alternative is every plugin building its own client.

### 4.5 Caller

```rust
pub struct Caller {
    pub user_id: String,     // virtual user id
    pub user_name: String,
    pub read_only: bool,     // read-only API key
}
```

- **`Caller`** implements `FromRequestParts<S> where Arc<dyn Host>: FromRef<S>`. It rejects with 401 when nobody is signed in.
- **`OptionalCaller`** is the same extractor without the rejection.
- **Read-only keys need no plugin code.** The global `enforce_read_only_api_keys` middleware already covers mounted plugin routes (§5.3).

No admin flag in v1. Moonfin's admin endpoints are out of scope, and whether Jellyswarrm player accounts can be admins needs its own decision.

## 5. Core changes in `jellyswarrm-proxy`

### 5.1 Registry (`src/plugins/mod.rs`)

```rust
pub(crate) static PLUGINS: &[&dyn Plugin] = &[
    #[cfg(feature = "plugin-trickplay")] &jellyswarrm_plugin_trickplay::PLUGIN,
    #[cfg(feature = "plugin-intro-skipper")] &jellyswarrm_plugin_intro_skipper::PLUGIN,
    #[cfg(feature = "plugin-kodi-sync-queue")] &jellyswarrm_plugin_kodi_sync_queue::PLUGIN,
    #[cfg(feature = "plugin-moonfin")] &jellyswarrm_plugin_moonfin::PLUGIN,
];
```

Startup checks, with a unit test for each: plugin ids are unique and match `^[a-z0-9-]+$`.

### 5.2 Conventions refactor (`src/plugins/conventions.rs`)

Today the ID rules are statics in `protocol/routes.rs` and `translation/field_matcher.rs`. They become a single merged value:

```rust
pub(crate) struct ActiveConventions {
    pub media_id_path_tags: Vec<&'static str>,
    pub user_id_path_tags: Vec<&'static str>,
    pub media_id_array_fields: FieldMatcher,
    pub media_id_map_key_fields: FieldMatcher,
}
pub(crate) fn conventions() -> &'static ActiveConventions  // LazyLock: CORE ∪ PLUGINS
```

- **Deterministic.** The compiled-in plugin set is fixed per build, so a `LazyLock` is safe: no startup ordering problems and no test cross-talk.
- **Core lists keep everything else.** The entries that move are listed in §6.
- **Call sites change mechanically** (`MEDIA_ID_PATH_TAGS` → `conventions().media_id_path_tags`):
  - `application/request/containers.rs`
  - `application/request/selection.rs`
  - `application/request/context.rs`
  - `application/request/collect/tests.rs`
  - `translation/url_processor.rs`
  - `translation/response_processor.rs`
  - `translation/request_analyzer.rs`
  - `translation/request_processor.rs`
  - `translation/delivery_urls.rs`
  - `translation/field_matcher.rs`
- **Lists no plugin extends stay as statics** (`ID_FIELDS`, `SESSION_FIELDS`, `USER_FIELDS`, `DELIVERY_URL_FIELDS`, …).

### 5.3 Routing (`src/plugins/routes.rs`, `http/router.rs`)

`http/router.rs::routes()` becomes:

```rust
Router::new()
    .merge(client_sessions::router())
    // ... existing explicit routes ...
    .merge(super::client_routes::router())
    .merge(crate::plugins::routes::router(&plugin_hosts))   // NEW, before the catch-all
    .route("/{*path}", any(forwarding::forward))
    .fallback(forwarding::forward)
```

- **ProxiedRoutes** are expanded into spellings by one core helper that mirrors `lowercase_routes!`. The macro lowercases the inner routes first and then clones the whole nest under a lowercase prefix (`crates/jellyswarrm-macros/src/lib.rs`). So today `/Videos/{id}/Trickplay/…`, `/Videos/{id}/trickplay/…`, `/videos/{id}/Trickplay/…` and `/videos/{id}/trickplay/…` all match. The helper must keep all four, and keep the case of captured wildcard values. Fully mixed-case spellings such as `/VIDEOS` don't match today and won't afterwards.
- **Plugin routers** are built by calling `plugin.router(host)`. When it returns `Some(r)`, the proxy mounts `r.with_state(host)`, which converts `Router<Arc<dyn Host>>` into `Router<AppState>`. Plugin crates use `jellyswarrm_macros::lowercase_routes!` themselves.
- **Conflicts fail at startup.** Overlapping routes panic in axum. A test that builds the production router with all features catches this in CI.
- **Plugin routes inherit everything in `build_router`:** URL prefix, sessions, read-only key middleware, CORS and tracing.
- **`build_router` needs the hosts.** `ProxyHost`s are built in `startup/composition.rs` and handed over through a new `AppState` field (`plugins: PluginHosts`). The test fixture builds them the same way.

**No plugin-specific fallback rules in the core.** A Moonfin Seerr login body carries `Username` and `Password`. When Moonfin isn't mounted, the catch-all forwards `POST /Moonfin/Seerr/Login` to a backend, exactly as it does today. If forwarding credential-bearing bodies to backends needs restricting, that's a general forwarder rule and a separate change, not a list of Moonfin paths.

Today `/Videos/{stream_id}/Trickplay/{*path}` is a literal in `client_routes::router()`, inside the `/Videos` nest. The plugin can't sit inside that nest. A top-level route `/Videos/{stream_id}/Trickplay/{*path}` merged next to it should still work in axum 0.8, because a nest is flattened into the same matcher. **Verify this first.** If it conflicts, add a small `client_routes::extend_videos(...)` hook so the `/Videos` nest takes plugin routes.

### 5.4 Host implementation (`src/plugins/host.rs`)

`ProxyHost { plugin_id, media: MediaServices, authorization: Arc<UserAuthorizationService>, settings: Option<Value>, store: SqlitePluginStore }`.

- **`caller()`** wraps `http::incoming::resolve_request_identity` and maps `RequestIdentity` to `Caller`. Bare `Authorization: MediaBrowser Token="…"` already parses through `Authorization::parse_request_header` (`protocol/authorization.rs:31`). Pin that with a test; the fork's header rewrite should not be needed.
- **`settings()`** returns the plugin's `[plugins.<id>]` table (§5.6).

### 5.5 Storage (`src/plugins/store.rs`, new migration)

`migrations/20261128000000_plugin_data.up.sql`:

```sql
CREATE TABLE plugin_global_data (
    plugin_id  TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,          -- JSON, or ciphertext when encrypted = 1
    encrypted  INTEGER NOT NULL DEFAULT 0,
    updated_at TIMESTAMP NOT NULL,
    PRIMARY KEY (plugin_id, key)
);
CREATE TABLE plugin_user_data (
    plugin_id  TEXT NOT NULL,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    encrypted  INTEGER NOT NULL DEFAULT 0,
    updated_at TIMESTAMP NOT NULL,
    PRIMARY KEY (plugin_id, user_id, key)
);
```

- **Two tables** so user data cascades on user delete. A single table with a nullable `user_id` wouldn't work: SQLite treats NULLs in a primary key as distinct.
- **Secrets use a per-plugin key.** It's derived with the existing HKDF helper in `domain/security.rs` (purpose `b"jellyswarrm-plugin-secret:" + plugin_id`) and encrypted with AES-GCM like mapping passwords.
- **Plugins never ship migrations.** The core migration owns the schema. If a plugin's data no longer fits a key-value store, we revisit.

### 5.6 Settings (`config/mod.rs`)

Add a `plugins` table to `AppConfig`:

```rust
#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
pub(crate) plugins: BTreeMap<String, toml::Table>,
```

```toml
[plugins.moonfin]
seerr_url = "http://seerr:5055"
seerr_display_name = "Requests"
```

- **Env overrides work already** through the existing `__` separator: `JELLYSWARRM_PLUGINS__MOONFIN__SEERR_URL`. Add a config test.
- **Values are read once at startup.** PR 4 shows the settings read-only in the admin UI, and changes need a restart, like `login_idle_expiry_days`.
- **Docs:** `docs/config.md` gets a "Plugins" section, and each plugin documents its own keys in its crate README.

## 6. Per-plugin specs

### 6.1 trickplay

```rust
const CONVENTIONS: Conventions = Conventions { media_id_map_key_fields: &["Trickplay"], ..Conventions::EMPTY };
const ROUTES: &[ProxiedRoute] = &[ProxiedRoute {
    method: GET, nest: Some("/Videos"), path: "/{stream_id}/Trickplay/{*path}", behavior: Behavior::Forward,
}]; // the core expands this to the four existing spellings (§4.3, §5.3)
```

Core removals:
- the route at `http/client_routes/mod.rs:94`
- `MEDIA_ID_MAP_KEY_FIELDS` in `translation/field_matcher.rs:106` (the core list becomes empty)

Core keeps:
- the `trickplay` model field (`protocol/models/jellyfin/mod.rs:314`), which is plain data
- the `"Trickplay"` string in `application/catalog/episode_queue.rs:69`, which only recognises the stock web request

When compiled out:
- `/Videos/{id}/Trickplay/*` falls to `/Videos/{stream_id}/{*path}` → `video_resource`. `tiles.m3u8` then goes through HLS playlist handling.
- `Trickplay` keys are no longer translated.
- A `#[cfg(not(feature = "plugin-trickplay"))]` test pins this fall-through, so it's deliberate rather than accidental.

Tests that move behind `#[cfg(feature = "plugin-trickplay")]`:
- `http_tests/resources.rs:30-31`
- the `Trickplay` assertions in `application/playback/response.rs` and `application/catalog/item_store.rs:953`

### 6.2 intro-skipper

```rust
const CONVENTIONS: Conventions = Conventions { media_id_path_tags: &["MediaSegments"], ..Conventions::EMPTY };
const ROUTES: &[ProxiedRoute] = &[ProxiedRoute {
    method: GET, nest: None, path: "/MediaSegments/{item_id}", behavior: Behavior::ItemList,
}]; // expanded to /MediaSegments/… and /mediasegments/…
```

Core removals:
- the route at `http/client_routes/mod.rs:71`
- `"MediaSegments"` in `protocol/routes.rs:12`

When compiled out, `/MediaSegments/{id}` goes to the catch-all forwarder. Without the path tag the ID isn't rewritten and the default backend returns 404/empty, so clients simply show no skip button.

Tests behind the feature: `http_tests/catalog.rs:72` and `:103`.

Legacy Intro Skipper routes (`/Episode/{id}/IntroTimestamps[/v1]`, `/Episode/{id}/IntroSkipperSegments`) are gone from current Intro Skipper (its controllers are admin-only now). Add them here as `Forward` + an `Episode` path tag only if a client turns out to still call them.

### 6.3 kodi-sync-queue

```rust
const CONVENTIONS: Conventions = Conventions {
    user_id_path_tags: &["Jellyfin.Plugin.KodiSyncQueue"],
    media_id_array_fields: &["ItemsAdded", "ItemsUpdated", "ItemsRemoved"],
    ..Conventions::EMPTY
};
```

No routes; the catch-all already forwards `GET /Jellyfin.Plugin.KodiSyncQueue/{userId}/GetItems`.

Core removals:
- the entry in `protocol/routes.rs:52`
- the three fields in `translation/field_matcher.rs:91-94`

Tests that move behind the feature:
- the KodiSyncQueue cases in `translation/url_processor.rs:681-682`
- `http_tests/kodi.rs` `kodi_sync_queue_translates_user_and_changed_item_ids` and its `WORKFLOWS` entry in `http_tests/coverage.rs:40`

Other Kodi tests (QuickConnect and playback) stay in the core, because they aren't about the plugin.

### 6.4 moonfin

Dependencies: `jellyswarrm-plugin-api`, `axum`, `reqwest`, `serde`, `serde_json`, `tracing`, `chrono`, `jellyswarrm-macros`.

**Settings** (`[plugins.moonfin]`):

| Key | Default | Meaning |
|---|---|---|
| `seerr_url` | — | Seerr base URL as reachable from Jellyswarrm. Unset → `router()` returns `None` and all `/Moonfin/*` requests fall through. A malformed URL is logged as an error and also leaves Moonfin unmounted. |
| `seerr_display_name` | `"Seerr"` | Label shown in clients |

**Requirement:** Seerr must use Jellyswarrm itself as its Jellyfin server. Moonfin's Seerr login calls Seerr's Jellyfin auth with the user's Jellyswarrm credentials.

**Routes** (declared with `lowercase_routes!` inside a `/Moonfin` nest, so they accept the same spellings as core routes; wildcard values keep their case):

| Method | Path | Auth | Source contract |
|---|---|---|---|
| GET | `/Ping` | `Caller` | `MoonfinController.Ping` |
| GET | `/Discovery`, `/Discovery/discover` | none | `MoonfinDiscoveryController` |
| GET | `/Seerr/Config` | `Caller` | `MoonfinController.GetSeerrConfig` |
| POST | `/Seerr/Login` | `Caller` | `SeerrProxyController.Login` |
| GET | `/Seerr/Status` | `Caller` | `.Status` |
| GET | `/Seerr/Validate` | `Caller` | `.Validate` |
| DELETE | `/Seerr/Logout` | `Caller` | `.Logout` |
| GET/POST/PUT/DELETE | `/Seerr/Api/{*path}` | `Caller` | `.ProxyGet/Post/Put/Delete` |
| same | `/Jellyseerr/*` | | legacy alias, same handlers |

- **Only the routes above are owned.** Unimplemented paths such as Settings, MediaBar and Games fall through to default routing, per the "disabled means absent" rule. No catch-all 404 inside the prefix.
- **Ping must only claim what we serve:** `installed`, `version`, `serverName`, `seerrEnabled`, `seerrUrl`, with `mdblistAvailable`/`tmdbAvailable`/`messagesSupported`/`recommendationsSupported` set to `false`. Omit `settingsSyncEnabled` entirely: the fork found that an explicit `false` makes clients turn Seerr off.
- **Without a Seerr URL**, Moonfin mounts nothing, so Ping, Discovery and Seerr all fall through to the backend as they do today. Instances that don't set Moonfin up keep reaching a backend that runs the real Moonfin plugin. Once settings sync exists (PR 5), the mount condition becomes "any Moonfin feature is configured".
- **Discovery** returns `[{id, name: server_name, address: <scheme://host + url_prefix>, type: "Jellyfin"}]` with no-cache headers. The address comes from the request plus the configured prefix, using the host’s `url_prefix`.

**State.** Store one encrypted JSON record at `(moonfin, caller.user_id, seerr_session)`. Include the cookie name and value, Seerr user ID, display name, avatar, permissions, timestamps and normalized Seerr base URL (scheme, host, effective port and base path). Persist a successful login with one SQLite upsert. Failed logins leave the preceding record intact. Logout deletes the whole row. Read the complete session once per operation. A changed base URL requires a fresh login before any cookie is sent. Confine authenticated API requests and redirects to the configured service. Validation may refresh the stored display name, avatar and permissions (the fork does this), but only with a conditional write: the update applies only if the record is unchanged since it was read (compare `created_at` or a version counter). A delayed validation response therefore can't overwrite or clear a newer login. The store needs a `compare_and_put` for this (§4.4).

**Porting source:** `asenmitrev/Jellyswarrm` branch `moonfin-seerr`, `crates/jellyswarrm-proxy/src/handlers/moonfin.rs` (689 lines) and `moonfin/tests.rs` (541 lines). The port:
- replaces `AppState`/`User` with `Arc<dyn Host>`/`Caller`
- replaces `seerr_sessions.rs` with `PluginStore`
- drops `token_only_authorization` if the core parser test in §5.4 passes
- replaces the fork's config fields with `[plugins.moonfin]`

Before starting, ask asenmitrev whether they want to do the port or co-author it (they offered in #195).

**Tests:**
- in-crate unit tests: a fake `Host` with an in-memory store, plus wiremock standing in for Seerr
- production-router tests with Seerr configured: both login aliases (`Seerr`, `Jellyseerr`) use the local handler and send nothing to the Jellyfin backends, a read-only key gets 403 on login, Discovery works with a URL prefix, and an unknown `/Moonfin/Foo` still reaches the selected backend
- production-router test with Seerr unset: `/Moonfin/Ping` is forwarded to the backend, the same as a build without Moonfin
- session tests: user isolation, failed and concurrent login, logout, validation racing with a new login, and URL changes across restart. After a URL change the new Seerr receives no old cookie
- dev stack: `just up` already starts Seerr (`--profile seerr`), so add a manual check to `dev/README.md`

## 7. Testing and CI

- **Route inventory (`http_tests/coverage.rs`).** The client inventory includes `/MediaSegments/{id}` and `/Videos/{id}/Trickplay/...`.
  - When their plugin is compiled in, the plugin's contracts cover them, because `resources::ROUTES` / `catalog::ROUTES` entries move behind the same `cfg`.
  - When it's compiled out, a `PLUGIN_GATED` list classifies them as "falls through to default routing (plugin compiled out)", so the unclassified-route check still passes.
- **Router composition test:** build the production router with every compiled-in plugin, so route conflicts panic in CI.
- **Registry tests:** ids are unique and valid, and every plugin's conventions contain no duplicates of core entries. Duplicates would be harmless but point to a missed core removal.
- **CI (`.github/workflows/ci.yml`):**
  - existing `cargo check/clippy --all-features` and the default workspace tests stay
  - add `cargo test --locked -p jellyswarrm-proxy --no-default-features`
  - add `cargo clippy --locked -p jellyswarrm-proxy --no-default-features --all-targets -- -D warnings`
  - optional, later: a matrix with each plugin alone (`--no-default-features --features plugin-x`) if per-plugin `cfg` bugs show up
- **`just` recipe:** `just test-no-plugins` for local use.

## 8. PR breakdown

Each PR is reviewable and shippable on its own. PRs 1 and 2 preserve existing client behaviour, and PR 3 changes nothing for instances without a Seerr URL.

**PR 1: plugin API crate, registry, conventions refactor**
- Add `jellyswarrm-plugin-api` (Plugin, Conventions, ProxiedRoute, Behavior).
- Add `src/plugins/{mod,conventions,routes}.rs` and convert only consumers of the four lists extended by the adapters. Keep query tags and scalar response ID fields core-only. Add the custom router hook, Host and Caller in PR 3, and persistence in PR 4.
- Mount ProxiedRoutes before the catch-all.
- Add the `--no-default-features` CI jobs.
- *Done when:* all existing tests pass; registry is empty; no behaviour change.

**PR 2: move trickplay, intro-skipper and kodi-sync-queue into crates**
- Move the routes, conventions and tests (§6.1–6.3), and add the `PLUGIN_GATED` coverage list.
- Add the fall-through tests for each plugin when it's compiled out. With trickplay enabled, request a manifest through all four path spellings on the production router. Check the owning backend, Range header, response status and exact bytes to distinguish ordinary forwarding from playlist processing.
- *Done when:* default and no-default tests pass, default routing preserves the existing contracts, and each adapter owns its production route descriptors and ID rules.

**PR 3: host services and the Moonfin crate (Ping, Discovery)**
- Add the host and caller interfaces, `[plugins.<id>]` config, `AppState.plugins`, and plugin router mounting.
- Add the Moonfin crate to the default features with Ping and Discovery. It mounts only when `seerr_url` is set, and Ping reports `seerrEnabled: false` until PR 4 lands, so with no Seerr URL nothing changes.
- Tests: `Caller` with every token form (including the bare `MediaBrowser Token="…"`), env override of plugin settings, fall-through with `seerr_url` unset.
- *Done when:* with `seerr_url` set, the production router serves authenticated Ping and anonymous Discovery; with it unset, `/Moonfin/*` reaches the backend.

**PR 4: Moonfin Seerr integration**
- Add the store and migration when Seerr first needs them. Test the single-record session replacement, `compare_and_put`, user deletion and Seerr URL changes. Complete the Seerr port in §6.4 and write `docs/plugins.md` (what each plugin does, config, behaviour when compiled out or unconfigured).
- Show the Moonfin settings read-only on the admin Settings page, with a "restart to apply" note.
- *Done when:* Moonfin client against the dev stack can log in to Seerr and make a request; unit and wiring tests pass.

**PR 5 (later): Moonfin settings sync**
- `GET/POST/DELETE /Settings`, `/Settings/Resolved/{profile}`, `/Settings/Profile/{profile}`, `/Defaults` on `Scope::User`; Ping then advertises `settingsSyncEnabled: true`.
- Admin-only endpoints stay out until there's a decision on player admins.

## 9. Later extensions (not planned work)

- **`Behavior` variants for more core workflows**, if an adapter needs e.g. fan-out to all servers (`all_servers`).
- **Catalog access on `Host`**, for Moonfin's `Genres`, `Items/{id}/Similar` and `MediaBar`: federated item queries returning virtual IDs.
- **`Plugin::web_injections()`**, for scripts or CSS injected into the served `index.html` (#244).
- **An HTTP sidecar plugin**, implementing `Plugin` by forwarding a configured prefix to an external service with `Caller` headers. That would cover asenmitrev's `generic-plugins` idea without core changes.

## 10. Risks and open points

- **Trickplay route placement (§5.3).** Confirm that the top-level merge doesn't conflict with the `/Videos` nest before writing PR 2.
- **Conventions `LazyLock` in tests.** Some unit tests may assert core behaviour that a plugin's rules now change (e.g. response-processor tests over `Trickplay`). Gate those tests on the feature rather than weakening them.
- **Moonfin contract drift.** Moonfin is very active (pushed 2026-11-25). Pin the upstream commit the port targets in the crate README and recheck Ping fields when we bump.
- **Restart-to-apply settings.** This is acceptable for now. Live reload would conflict with "unconfigured means absent" (the router would have to stay mounted and check config per request), and it must keep the session-to-Seerr-URL binding from §6.4.
