# Proxy architecture

Jellyswarrm presents several Jellyfin servers as one. This page maps the
top-level modules so you know where to look; see
[request and data flow](DATA_FLOW.md) for how a request moves through them.

All module paths below are relative to `src/`.

## Startup and incoming requests

| Module | Purpose |
|---|---|
| `startup/` | Starts the process: loads configuration, constructs services, assembles both routers and owns background tasks and shutdown. |
| `http/` | Handles the client API, authentication extraction, WebSocket connections, embedded web assets and HTTP responses. `http/state.rs` holds the service handles handlers need. |
| `ui/` | Serves the management pages, forms and UI login. It is separate from the client API because its presentation and authentication needs differ. |
| `application/` | Coordinates catalog, playback, account and administration operations. It decides which backends a caller may use and combines the services needed to complete a request. Steps every workflow shares (caller context, backend routing, upstream request building) live in `application/request/`; server and library administration in `application/admin/`. |

`main.rs` supplies the Tokio runtime and calls the library's only public entry
point, `run()`. Startup wires the modules together.

## Feature services

| Module | Purpose |
|---|---|
| `auth/` | Local users, credentials, API keys, backend account mappings and Quick Connect. |
| `servers/` | Backend registration, addresses, health and stable server ordering. |
| `libraries/` | Virtual library groups and their backend members. |
| `media/` | Media identity, virtual ID mappings, versions and catalog inventory. |
| `playback.rs` | Playback-session and stream bindings that keep later requests on the right backend. |
| `sessions/` | Connected client presence, observed playback state and bounded message delivery. |
| `syncplay/` | Group membership, shared queues and synchronized playback control. |

Playback bindings and client sessions are separate: one tracks **where media is
playing**, the other tracks **which clients are connected**.

## Translation, transport and storage

| Module | Purpose |
|---|---|
| `processors/` | Inspects and translates JSON, URLs and identifiers between the virtual server and a selected backend, including JSON body framing. |
| `upstream/` | Makes backend requests and delivers JSON, streams and HLS playlists. |
| `persistence/` | Opens SQLite, runs migrations and converts shared values to database representations. SQL also still lives in feature services. |
| `config.rs`, `config/` | Loads configuration, provides startup and runtime settings, and serializes save/reload operations. |

Handlers read runtime settings through `SettingsView`; configuration changes go
through `ConfigurationService`.

## Shared vocabulary

| Module | Purpose |
|---|---|
| `domain/` | Shared IDs and security values. |
| `models/` | Jellyfin request and response data shapes, including compatibility fields. |
| `protocol/` | Authorization/device parsing and Jellyfin route and URL conventions. |

## Following a request

A catalog request enters through `http/`, becomes an application operation, then
uses feature services, processors and upstream transport to produce a response.
A remote-control request instead uses application session checks, SyncPlay's
group rules and session delivery before the WebSocket adapter sends the message.
See [request and data flow](DATA_FLOW.md) for concrete paths.
