# Proxy architecture

Jellyswarrm presents several Jellyfin servers as one. This page maps the
top-level modules so you know where to look; see
[request and data flow](DATA_FLOW.md) for how a request moves through them.

All module paths below are relative to `src/`.

## Where code for a feature goes

Most features are split into up to three tiers, with the same module name in
each tier so you can find all of them by name:

| Tier | Location | Owns |
|---|---|---|
| Adapter | `http/<feature>` or `ui/` | Axum extraction, status codes and rendering. One handler calls one application workflow. |
| Workflow | `application/<feature>` | Steps that combine several feature services, access checks and upstream requests for one operation. |
| Feature state | `<feature>/` (or a submodule of `auth/`, `media/`) | Owned state, storage and rules for one concept. No HTTP types, no other features' workflows. |

| Feature | Adapter | Workflow | Feature state |
|---|---|---|---|
| Forwarding (every route without a dedicated workflow) | `http/forwarding.rs` | `application/forwarding.rs` | (none) |
| Quick Connect | `http/quick_connect.rs` | `application/quick_connect.rs` | `auth/quick_connect/` |
| Client sessions, remote control | `http/client_sessions/` | `application/client_sessions/` | `client_sessions/` |
| SyncPlay | `http/syncplay.rs` | `application/syncplay.rs` | `syncplay/` |
| Playback | `http/client_routes/` | `application/playback/` | `playback.rs` |
| Catalog and item details | `http/client_routes/` | `application/catalog/`, `application/details/` | `media/` |
| Libraries, servers (admin) | `ui/admin/` | `application/admin/` | `libraries/`, `servers/` |

Add a workflow tier only when an operation needs more than one feature service;
simple reads may go straight from an adapter to feature state. Dependencies point
down this table: feature state never imports `application`, `http` or `ui`.

Tests follow the code they exercise. Small tests stay inline; larger suites live
in a `tests.rs` (or a named `*_tests.rs`) next to their owning `mod.rs`. HTTP
contract tests are in `http_tests/` and real-server/browser tests in `../tests/`.

## Startup and incoming requests

| Module | Purpose |
|---|---|
| `startup/` | Starts the process: loads configuration, constructs services, assembles both routers and owns background tasks and shutdown. |
| `http/` | Handles the client API, authentication extraction, WebSocket connections, embedded web assets and HTTP responses. `http/state.rs` holds the service handles handlers need. |
| `ui/` | Serves the management pages, forms and UI login. It is separate from the client API because its presentation and authentication needs differ. |
| `application/` | Coordinates catalog, playback, account and administration operations. It decides which backends a caller may use and combines the services needed to complete a request. Steps every workflow shares (collecting the caller context, backend routing, target authorization, upstream request building) live in `application/request/`; the media service handles they use in `application/media_services.rs`. |

`main.rs` supplies the Tokio runtime and calls the library's only public entry
point, `run()`. Startup wires the modules together.

## Feature services

| Module | Purpose |
|---|---|
| `auth/` | Local users, credentials, API keys, backend authorization sessions (`auth/service/authorization_sessions.rs`), backend account mappings and Quick Connect. |
| `servers/` | Backend registration, addresses, health and stable server ordering. |
| `libraries/` | Virtual library groups and their backend members. |
| `media/` | Media identity, grouping and deduplication rules, virtual ID mappings, versions and catalog inventory. |
| `playback.rs` | Playback-session and stream bindings that keep later requests on the right backend. |
| `client_sessions/` | Connected client presence, observed playback state and bounded message delivery. |
| `syncplay/` | Group membership, shared queues and synchronized playback control. |

Three kinds of "session" exist and are kept apart by name:
**authorization sessions** (`auth`) hold backend tokens per device and server,
**playback sessions** (`playback.rs`) track *where media is playing*, and
**client sessions** (`client_sessions/`) track *which clients are connected*.

## Translation, transport and storage

| Module | Purpose |
|---|---|
| `translation/` | Inspects and translates JSON, URLs and identifiers between the virtual server and a selected backend. |
| `upstream/` | Makes backend requests and delivers JSON, streams and HLS playlists. |
| `persistence/` | Opens SQLite, runs migrations and converts shared values to database representations. SQL also still lives in feature services. |
| `config/` | Loads configuration, provides startup and runtime settings, and serializes save/reload operations. |

Handlers read runtime settings through `SettingsView`; configuration changes go
through `ConfigurationService`.

## Shared vocabulary

| Module | Purpose |
|---|---|
| `domain/` | Shared IDs and security values. |
| `protocol/` | Jellyfin wire conventions: request/response DTOs (`protocol/models/`), authorization headers, caller and device identity, JSON body framing, route classification and URL conventions. |

## Following a request

Most client requests have no dedicated route: the catch-all
`http/forwarding.rs` sends them through `application/forwarding.rs` to a single
backend. Start there to see the proxy's core path.

A catalog request enters through `http/`, becomes an application operation, then
uses feature services, translation and upstream transport to produce a response.
A remote-control request instead uses application session checks, SyncPlay's
group rules and session delivery before the WebSocket adapter sends the message.
See [request and data flow](DATA_FLOW.md) for concrete paths.
