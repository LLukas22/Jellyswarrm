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
| `server_id` | `jellyswarrm{20-char-uuid}` | `JELLYSWARRM_SERVER_ID` | Unique identifier for the proxy server instance. |
| `public_address` | `localhost:3000` | `JELLYSWARRM_PUBLIC_ADDRESS` | Public address where the proxy is accessible. |
| `server_name` | `Jellyswarrm Proxy` | `JELLYSWARRM_SERVER_NAME` | Display name for the proxy server. |
| `host` | `0.0.0.0` | `JELLYSWARRM_HOST` | Host address the server binds to. |
| `port` | `3000` | `JELLYSWARRM_PORT` | Port number for the proxy server. |
| `include_server_name_in_media` | `true` | `JELLYSWARRM_INCLUDE_SERVER_NAME_IN_MEDIA` | Append the server name to media titles in responses. |
| `username` | `admin` | `JELLYSWARRM_USERNAME` | Default admin username. |
| `password` | `jellyswarrm` | `JELLYSWARRM_PASSWORD` | Default admin password (⚠️ change this in production). |
| `session_key` | *Generated 64-byte key* | `JELLYSWARRM_SESSION_KEY` | Base64-encoded session encryption key. |
| `timeout` | `20` | `JELLYSWARRM_TIMEOUT` | Request timeout in seconds. |
| `ui_route` | `ui` | `JELLYSWARRM_UI_ROUTE` | URL path segment for accessing the web UI (e.g., `/ui`). |
| `url_prefix` | *(none)* | `JELLYSWARRM_URL_PREFIX` | Optional URL prefix for all routes (useful for reverse proxy setups). |

---

### Notes
- The `session_key` is generated as a secure 64-byte key if not specified, and is stored in the config file for reuse.  
