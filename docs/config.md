# Jellyswarrm Configuration Documentation  

Jellyswarrm stores its configuration in a **TOML** file located at:  
`./data/jellyswarrm.toml` (inside the container).  

The SQLite database is stored at:  
`./data/jellyswarrm.db`.  

To persist your configuration and database across container restarts, mount a volume to the `./data` directory.  

You can override the default configuration in two ways:  
1. Provide your own `jellyswarrm.toml` file and mount it into the container.  
2. Use environment variables to override individual settings.  

---

## Configuration Options  

The table below lists all available configuration options:  

| Variable | Default Value | Environment Key | Description |
|----------|---------------|-----------------|-------------|
| `server_id` | *Generated UUID (32 hex chars)* | `JELLYSWARRM_SERVER_ID` | Unique identifier for the proxy server instance. |
| `public_address` | `localhost:3000` | `JELLYSWARRM_PUBLIC_ADDRESS` | Public address where the proxy is accessible. |
| `server_name` | `Jellyswarrm Proxy` | `JELLYSWARRM_SERVER_NAME` | Display name for the proxy server. |
| `host` | `0.0.0.0` | `JELLYSWARRM_HOST` | Host address the server binds to. |
| `port` | `3000` | `JELLYSWARRM_PORT` | Port number for the proxy server. |
| `include_server_name_in_media` | `true` | `JELLYSWARRM_INCLUDE_SERVER_NAME_IN_MEDIA` | Append the server name to media titles in responses. |
| `username` | `admin` | `JELLYSWARRM_USERNAME` | Default admin username. |
| `password` | `jellyswarrm` | `JELLYSWARRM_PASSWORD` | Default admin password (⚠️ change this in production). |
| `session_key` | *Generated 64-byte key* | `JELLYSWARRM_SESSION_KEY` | Base64-encoded session encryption key. |
| `timeout` | `20` | `JELLYSWARRM_TIMEOUT` | Request timeout in seconds. |
| `preconfigured_servers` | `[]` | `JELLYSWARRM_PRECONFIGURED_SERVERS` | Optional list of preconfigured Jellyfin servers (`url`, `name`, `priority`, `media_streaming_mode`). |
| `ui_route` | `ui` | `JELLYSWARRM_UI_ROUTE` | URL path segment for accessing the web UI (e.g., `/ui`). |
| `url_prefix` | *(none)* | `JELLYSWARRM_URL_PREFIX` | Optional URL prefix for all routes (useful for reverse proxy setups). |
| `server_background_check_interval_secs` | `30` | `JELLYSWARRM_SERVER_BACKGROUND_CHECK_INTERVAL_SECS` | Interval in seconds for background server health checks. |
| `auto_create_users_on_login` | `true` | `JELLYSWARRM_AUTO_CREATE_USERS_ON_LOGIN` | Automatically create local users on successful upstream login. |
| `login_idle_expiry_days` | `0` | — | Days of inactivity before a player must sign in again. Zero disables expiry. Named API keys have independent backend sessions and do not expire. Restart after changing this setting. |
| `preserve_upstream_device_ids` | `false` | `JELLYSWARRM_PRESERVE_UPSTREAM_DEVICE_IDS` | Set to `true` and restart if a backend requires approved client device IDs. By default each login gets its own backend device ID. With preservation enabled, Jellyfin can revoke an earlier backend token when the same device signs in again. |
| `merge_libraries` | `true` | `JELLYSWARRM_MERGE_LIBRARIES` | Merge libraries with matching names across servers into virtual libraries. |
| `deduplicate_media` | `true` | `JELLYSWARRM_DEDUPLICATE_MEDIA` | Collapse the same movie or show (series/season/episode) on multiple servers into one item whose versions are served by the different hosts (Jellyfin-style linked versions; Jellyfin v12 adds multi-versions for episodes). Also merges the home feeds: *Recently Added*, *Continue Watching* (resumes from the most recently played version) and *Next Up* (one entry per matched show, at the furthest episode any server reports). Legacy key `deduplicate_movies` / env `JELLYSWARRM_DEDUPLICATE_MOVIES` still loads. |
| `catalog_cache_size_mb` | `256` | `JELLYSWARRM_CATALOG_CACHE_SIZE_MB` | Estimated cache memory budget in MiB. Active scans use additional memory. `0` disables caching. Requires a restart. |
| `catalog_cache_fresh_secs` | `300` | `JELLYSWARRM_CATALOG_CACHE_FRESH_SECS` | Seconds before an accessed listing is refreshed in the background. Requires a restart. |
| `catalog_cache_max_age_secs` | `21600` | `JELLYSWARRM_CATALOG_CACHE_MAX_AGE_SECS` | Maximum age in seconds for a cached listing. At least `catalog_cache_fresh_secs`. Requires a restart. |

---

### Notes
- The `session_key` is generated as a secure 64-byte key if not specified, and is stored in the config file for reuse.  
- Each server now has its own streaming mode (`Redirect` or `Proxy`). For preconfigured servers, omit `media_streaming_mode` to use the default `Redirect`.
- An environment variable overrides the top-level key of the same name, e.g. `JELLYSWARRM_SERVER_BACKGROUND_CHECK_INTERVAL_SECS`. A double underscore reaches a nested key, e.g. `JELLYSWARRM_DEBUG_USER__USERNAME`.
- Configuration files are resolved from the data directory (`./data` by default), which can be overridden with `JELLYSWARRM_DATA_DIR`.

Upgrading to per-login credentials signs players out once. Existing named API keys keep their values and authenticate each mapped backend on first use. For Jellyfin device allowlists, approve the integration device `jellyswarrm-key-<key id>` for password mappings. A Quick Connect mapping uses its own durable credential. Placeholder player device IDs always receive fresh upstream IDs. When a real ID is preserved, Jellyfin may retire the previous upstream token even if the proxy cannot finish saving its replacement.

If Jellyfin rejects a saved password during automatic API-key sign-in, Jellyswarrm pauses automatic sign-in for that mapping across all keys and restarts. Update the mapping credentials to resume. When a key already has a healthy backend session, missing healthy backends sign in in the background.
