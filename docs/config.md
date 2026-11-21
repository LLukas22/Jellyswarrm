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
| `merge_libraries` | `true` | `JELLYSWARRM_MERGE_LIBRARIES` | Merge libraries with matching names across servers into virtual libraries. |
| `deduplicate_media` | `true` | `JELLYSWARRM_DEDUPLICATE_MEDIA` | Collapse the same movie or show (series/season/episode) on multiple servers into one item whose versions are served by the different hosts (Jellyfin-style linked versions; Jellyfin v12 adds multi-versions for episodes). Also merges the home feeds: *Recently Added*, *Continue Watching* (resumes from the most recently played version) and *Next Up* (one entry per matched show, at the furthest episode any server reports). Legacy key `deduplicate_movies` / env `JELLYSWARRM_DEDUPLICATE_MOVIES` still loads. |

---

### Notes
- The `session_key` is generated as a secure 64-byte key if not specified, and is stored in the config file for reuse.  
- Each server now has its own streaming mode (`Redirect` or `Proxy`). For preconfigured servers, omit `media_streaming_mode` to use the default `Redirect`.
- Configuration files are resolved from the data directory (`./data` by default), which can be overridden with `JELLYSWARRM_DATA_DIR`.

---

### Unauthenticated requests

Requests that have no dedicated route in Jellyswarrm go to the catch-all and are forwarded to a backend server. Requests that carry a Jellyfin access token are forwarded for any path. The token can be in `Authorization` or `X-Emby-Authorization` with `Token=`, in `X-Emby-Token` or `X-MediaBrowser-Token`, or in the `api_key` query parameter. Requests **without** a token are forwarded only when the path is one of the endpoints Jellyfin serves anonymously. Every other unauthenticated request gets a local `404` and never reaches a backend. This stops internet scanners from probing backends through the proxy. Without it, backend admins can see the proxy's IP as the attacker, and fail2ban can ban it.

Default public routes (case-insensitive):

```
/System/Ping, /System/Info/Public, /Branding/**, /QuickConnect/**, /web/**,
/Items/*/Images/*/**, /Users/*/Images/*/**, /Persons/*/Images/*/**,
/Artists/*/Images/*/**, /Genres/*/Images/*/**, /MusicGenres/*/Images/*/**,
/Studios/*/Images/*/**
```

A client or plugin may make other unauthenticated calls. You can allow those with `JELLYSWARRM_EXTRA_PUBLIC_ROUTES`, a comma-separated list in the same syntax. `*` matches exactly one path segment, and a trailing `**` matches the rest of the path:

```bash
JELLYSWARRM_EXTRA_PUBLIC_ROUTES="/Intros/Public/**,/SomePlugin/*/Logo"
```

This setting is read from the environment at startup only.
