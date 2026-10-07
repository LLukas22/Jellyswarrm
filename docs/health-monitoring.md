# Health monitoring

Jellyswarrm exposes two unauthenticated monitoring endpoints:

* `GET /health` returns `200 OK` with `healthy` when the HTTP server is responding.
* `GET /ready` returns `200 OK` with `ready` after startup initialization and migrations
  have completed and the database is accessible. Database errors or a check taking
  longer than two seconds return `503 Service Unavailable` with `not ready`.

These endpoints always live at the root, even when `url_prefix` is configured, and
do not depend on the availability or configuration of upstream Jellyfin servers.

## Docker health check

The Docker image checks both endpoints every 30 seconds, with a 30-second startup
grace period and three retries. It uses port `3000` by default and honors
`JELLYSWARRM_PORT`. If you change the listening port only in the configuration file,
or bind to an address other than the wildcard/loopback address, override the
container health check to use the matching address and port.
